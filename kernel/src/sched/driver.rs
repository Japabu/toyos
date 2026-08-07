//! The kernel driver for the scheduler core — spec §6.2, §6.3, §7.5.
//!
//! This file is the whitelist of §3: percpu plumbing, the asm switch, the idle
//! loop, the trampoline. It decides nothing. Every scheduling decision, state
//! transition and ordering-sensitive step happens above it, in `toyos-sched`,
//! where the simulator drives the same code.
//!
//! The shape of a scheduler entry is fixed and total:
//!
//! ```text
//! preempt::disable()
//! drain device IRQ records into wakes
//! with_cpu(|cpu| SchedPass::begin(cpu, env, now).dispose_*().finish())
//! match action { Run(tok) => switch(tok), Resume => {}, Idle(tok) => halt }
//! preempt::enable_no_resched()
//! ```
//!
//! Everything after the switch belongs to whichever task resumes on this
//! stack, and there is nothing scheduler-related left to do there — no guard to
//! release, no outgoing task to park. That is what park-before-switch buys, and
//! it is sound only because a wake for the just-parked task is a *message to
//! this same CPU*, which cannot be consumed before the switch completes.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::arch::{asm, naked_asm};
use core::cell::UnsafeCell;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};

use toyos_sched::cpu::{Action, CpuHandle, CpuHandles, CpuSched, Env, SchedPass};
use toyos_sched::fair::Frontier;
use toyos_sched::hw::{CpuId, Hw, Kicker, Machine, Nanos};
use toyos_sched::mailbox::{mailbox, Kick, PreemptGuard, Urgency};
use toyos_sched::msg::Msg;
use toyos_sched::task::{RtState, TaskBuilder, TaskKey};
use toyos_sched::waitq::{Cancelled, Commit, CurrentTask};

use crate::arch::percpu;
use crate::hw::HW;
use crate::process::{OwnedAlloc, PageTables, TaskId, KERNEL_STACK_SIZE};

use super::payload::{
    KMsg, KShare, KShared, KWaitQueue, KernelCtx, KernelPayload, RawTicket, TaskHandle, ThreadSched,
};
use super::MAX_CPUS;

/// Proof that preemption is disabled for as long as the borrow lasts (spec
/// §7.2's N3). Constructible only by the two functions below, both of which
/// bracket it with the preempt count.
pub struct PreemptOff(());

// SAFETY: every constructor raises the kernel's preempt count first and lowers
// it only after the borrow ends, so the executing context cannot be
// descheduled while a value of this type is alive.
unsafe impl PreemptGuard for PreemptOff {}

/// Run `f` in a preempt-disabled region. Wake paths post mailbox messages from
/// here; a request raised inside is honoured on the way out, which is how an
/// RT wake reaches its own preemption.
pub fn preempt_off<R>(f: impl FnOnce(&PreemptOff) -> R) -> R {
    crate::preempt::disable();
    let result = f(&PreemptOff(()));
    crate::preempt::enable();
    result
}

static CPUS: AtomicPtr<CpuHandles<KMsg>> = AtomicPtr::new(ptr::null_mut());
static FRONTIER: Frontier = Frontier::new();
static NEXT_KEY: AtomicU64 = AtomicU64::new(1);

/// Per-CPU CPU-time counters, for `total_cpu_ns`. Cache-line padded.
#[repr(align(64))]
struct CpuTime(AtomicU64);
static CPU_TIME_NS: [CpuTime; MAX_CPUS] = [const { CpuTime(AtomicU64::new(0)) }; MAX_CPUS];

pub fn cpus() -> &'static CpuHandles<KMsg> {
    let ptr = CPUS.load(Ordering::Acquire);
    assert!(!ptr.is_null(), "scheduler used before sched::init");
    // SAFETY: set once by `init` from a leaked Box, never cleared.
    unsafe { &*ptr }
}

pub fn frontier() -> &'static Frontier {
    &FRONTIER
}

/// Monotonic and never reused, so a message about a dead task is provably
/// stale rather than ambiguously about its successor (spec §5.1). Deliberately
/// not `TaskId`: pids and tids are recycled.
fn next_key() -> TaskKey {
    TaskKey(NEXT_KEY.fetch_add(1, Ordering::Relaxed))
}

pub fn total_cpu_ns() -> u64 {
    (0..crate::arch::smp::cpu_count() as usize)
        .map(|i| CPU_TIME_NS[i].0.load(Ordering::Relaxed))
        .sum()
}

struct SchedSlot(UnsafeCell<Option<CpuSched<KernelPayload>>>);

// SAFETY: the cell is only ever reached through `with_cpu`, which indexes by
// the *calling* CPU's own id and refuses reentry. `CpuSched` itself is `!Sync`,
// so nothing it contains can escape into another CPU by any other route.
unsafe impl Sync for SchedSlot {}

static SCHEDS: [SchedSlot; MAX_CPUS] = [const { SchedSlot(UnsafeCell::new(None)) }; MAX_CPUS];
static IN_PASS: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

/// Is this CPU inside a pass? A nested pass is a bug, not something to defer —
/// but the preempt poll can legitimately ask, and the panic path must know
/// before it tries to rejoin.
pub fn in_pass() -> bool {
    IN_PASS[percpu::cpu_id() as usize].load(Ordering::Relaxed)
}

/// The only accessor. Panics on reentry: the busy flag is the typed
/// replacement for `IN_SCHEDULE`, and a nested pass would alias `&mut`.
fn with_cpu<R>(f: impl FnOnce(&mut CpuSched<KernelPayload>) -> R) -> R {
    let cpu = percpu::cpu_id() as usize;
    assert!(
        !IN_PASS[cpu].swap(true, Ordering::Acquire),
        "nested scheduler pass on cpu {cpu}",
    );
    // SAFETY: exclusive by the flag above, and by CpuId — no other CPU indexes
    // this slot.
    let sched = unsafe { (*SCHEDS[cpu].0.get()).as_mut() }
        .unwrap_or_else(|| panic!("cpu {cpu} has no CpuSched"));
    let result = f(sched);
    IN_PASS[cpu].store(false, Ordering::Release);
    result
}

/// A read-only peek for diagnostics that must not fail while a pass runs.
fn try_with_cpu<R>(f: impl FnOnce(&CpuSched<KernelPayload>) -> R) -> Option<R> {
    let cpu = percpu::cpu_id() as usize;
    if IN_PASS[cpu].load(Ordering::Relaxed) {
        return None;
    }
    // SAFETY: as `with_cpu`, and shared rather than exclusive.
    let sched = unsafe { (*SCHEDS[cpu].0.get()).as_ref() }?;
    Some(f(sched))
}

/// Build every CPU's mailbox and handle, and the BSP's `CpuSched`. Called once,
/// before any task exists.
pub fn init() {
    let count = crate::arch::smp::cpu_count() as usize;
    assert!(count <= MAX_CPUS, "cpu count {count} exceeds MAX_CPUS");
    let mut handles = Vec::with_capacity(count);
    for cpu in 0..count {
        let (tx, rx) = mailbox::<KMsg>();
        handles.push(CpuHandle::new(CpuId(cpu as u32), tx));
        // SAFETY: single-threaded boot; the APs have not joined yet.
        unsafe {
            *SCHEDS[cpu].0.get() = Some(CpuSched::new(CpuId(cpu as u32), rx, idle_ctx()));
        }
    }
    CPUS.store(
        Box::into_raw(Box::new(CpuHandles::new(handles))),
        Ordering::Release,
    );
}

/// The context a CPU runs on when it has nothing to do. Having one is what lets
/// a pass free the previous zombie — an idle CPU never stands on a dead task's
/// stack.
fn idle_ctx() -> KernelCtx {
    KernelCtx {
        rsp: 0,
        cr3: crate::mm::paging::kernel_cr3(),
        fs_base: 0,
        kernel_stack_top: 0,
        id: None,
        // Never read: a CPU can only switch *to* its idle context from a task,
        // and reaching a task means it switched away from idle first, which
        // wrote the real depth. The idle loop enters by jump, not by switch.
        preempt: 0,
    }
}

/// Least-loaded CPU by published ready count (spec §9.4), scanning from a
/// rotating start so that ties spread instead of piling on one CPU.
///
/// The rotation is load-bearing at boot and only there: `publish_load` runs at
/// the end of a pass, and the init programs are all spawned before any CPU has
/// run one, so every published load is still zero and a fixed scan order would
/// put the whole system on CPU 0. Balance would pull them apart eventually,
/// but "eventually" is measured in idle passes and boot has none to spare.
fn placement() -> CpuId {
    static ROTATE: AtomicU64 = AtomicU64::new(0);
    let count = crate::arch::smp::cpu_count();
    let start = (ROTATE.fetch_add(1, Ordering::Relaxed) % count as u64) as u32;
    let mut best = CpuId(start);
    let mut best_load = cpus().get(best).load();
    for offset in 1..count {
        let cpu = CpuId((start + offset) % count);
        let load = cpus().get(cpu).load();
        if load < best_load {
            best_load = load;
            best = cpu;
        }
    }
    best
}

/// Everything a new thread needs. `entry_rsp` points at the trampoline frame
/// `alloc_kernel_stack` built.
pub struct NewTask {
    pub id: TaskId,
    pub kernel_stack: OwnedAlloc,
    pub entry_rsp: u64,
    pub address_space: Option<PageTables>,
    pub fs_base: u64,
    pub share: Arc<KShare>,
}

/// Place a new task by message — never by reaching into the destination's
/// queue (spec §9.4). Returns what the process table keeps.
pub fn spawn(new: NewTask) -> ThreadSched {
    let cr3 = new
        .address_space
        .as_ref()
        .expect("spawn: task without an address space")
        .lock()
        .cr3();
    let kernel_stack_top = new.kernel_stack.ptr() as u64 + KERNEL_STACK_SIZE as u64;
    let ctx = KernelCtx {
        rsp: new.entry_rsp,
        cr3,
        fs_base: new.fs_base,
        kernel_stack_top,
        id: Some(new.id),
        // The one level `trampoline_entry` discharges before the first `iretq`.
        preempt: 1,
    };
    let handle = Arc::new(TaskHandle::new());
    let task = TaskBuilder {
        key: next_key(),
        share: new.share,
        ctx,
        ext: KernelPayload {
            id: new.id,
            kernel_stack: new.kernel_stack,
            address_space: new.address_space,
            handle: handle.clone(),
        },
        rt: RtState::default(),
    }
    .build(placement(), HW.now());
    let sched = ThreadSched {
        handle,
        shared: task.shared().clone(),
    };
    let dst = match task.shared().state() {
        toyos_sched::task::TaskState::InTransit(cpu) => cpu,
        state => panic!("a freshly built task is not in transit: {state:?}"),
    };
    preempt_off(|p| {
        if cpus()
            .get(dst)
            .post_owned(Msg::Adopt { task }, Msg::adopt_node, Urgency::Normal, p)
            == Kick::Send
        {
            HW.kick(dst);
        }
    });
    sched
}

pub enum Dispose {
    /// An IRQ-exit poll: the pass decides for itself whether the running task
    /// keeps the CPU.
    None,
    Yield,
    Exit,
}

/// The environment every pass runs against.
///
/// `steal` is the one policy bit in it, and it is on: an idle pass probes the
/// busiest CPU for work and a loaded pass answers probes from surplus (spec
/// §7.7, §9.4's pull half). Without it a task woken onto a busy CPU waits
/// there until the owner yields.
///
/// The guard comes in by reference because its lifetime is the pass's and it
/// belongs to the caller that raised the count.
fn env(preempt: &PreemptOff) -> Env<'_, crate::hw::KernelHw, PreemptOff> {
    Env {
        hw: &HW,
        cpus: cpus(),
        frontier: &FRONTIER,
        preempt,
        steal: true,
    }
}

/// Run one scheduler pass and execute its action.
///
/// The preempt count is raised here and lowered by whichever context comes back
/// on this stack: this one after the switch returns, or a fresh task's
/// trampoline. It balances per context, not per call — which is why the count
/// travels *with* the context across the switch (`Hw::switch`) instead of being
/// inherited by whoever lands on the CPU next.
pub fn pass(dispose: Dispose) {
    crate::preempt::disable();
    // A pass *is* the reschedule the request asks for, so it owns the clear —
    // and it must clear before it drains, so a request raised by this pass's
    // own wakes survives into the next poll. Without this the idle loop never
    // sleeps: a kick IPI to a halted CPU is taken in Ring 0, which sets
    // `need_resched` and nothing else, and the pre-halt recheck then finds the
    // request still standing on every iteration.
    crate::preempt::clear_need_resched();
    drain_irqs();
    let now = HW.now();
    let action = with_cpu(|cpu| {
        let pass = SchedPass::begin(cpu, env(&PreemptOff(())), now);
        if let Some(current) = pass.cpu().running() {
            check_stack_canary(current.ext());
            current.ext().handle.publish(current.acct(), None);
        }
        let disposed = match dispose {
            Dispose::None => pass.dispose_none(),
            Dispose::Yield => pass.dispose_yield(),
            Dispose::Exit => pass.dispose_exit(),
        };
        disposed.finish()
    });
    charge_cpu_time(now);
    with_cpu(|cpu| {
        if let Some(current) = cpu.running() {
            current.ext().handle.publish(current.acct(), Some(now));
        }
    });
    execute(action);
    crate::preempt::enable_no_resched();
}

/// A wait registration, holding preemption off for the whole window between
/// phase 1 and phase 2 of the §8.1 handshake.
///
/// The window is not preemptible, and the guard is what makes that true rather
/// than hoped for. `prepare_wait` publishes `Committing(cpu, gen)` and the
/// machine has no edge out of it except the commit or the cancel: `preempt`
/// asserts on `Running`, and *inventing* a `Committing → Ready` edge would be
/// worse than the assert, because a waker that pops the registration and finds
/// the word `Ready` reports `Claim::Lost` and moves on to the next waiter —
/// the registered task is then off the queue, unwoken, and about to park. That
/// is a lost wake, which is the one thing this protocol exists to remove.
///
/// This is *not* §8.1's residual commit-to-park window, which has to be
/// tolerated because a remote CPU can act between two of our own instructions.
/// Nothing remote is involved here: the only route into a pass mid-window is
/// this CPU's own `preempt::enable` slow path, reached from the guard drop of
/// any lock the re-check takes. A window whose only intruder is ourselves can
/// be closed, so it is.
///
/// The guard is owned rather than remembered: the two ways to consume a ticket
/// both discharge it, so "registered with preemption on" has no expression.
#[must_use = "a wait ticket must be blocked on or cancelled"]
pub struct Ticket<'q>(RawTicket<'q>);

impl<'q> Ticket<'q> {
    /// Phase 1: register the running thread on `queue`.
    ///
    /// The count goes up before the current task is even read: without it, a
    /// preemption between reading the task and registering it would leave
    /// `CurrentTask` naming a CPU the thread no longer runs on, and
    /// `begin_commit` asserts on exactly that.
    pub fn register(queue: &'q KWaitQueue) -> Self {
        crate::preempt::disable();
        let shared = current_shared().expect("prepare_wait: no running thread");
        let current = CurrentTask::new(&shared, current_cpu());
        Self(queue.prepare_wait(&current))
    }

    /// The condition became true after registering: withdraw, and take the
    /// deferred preemption now that the thread is plainly `Running` again.
    pub fn cancel(self) -> Cancelled {
        let outcome = self.0.cancel();
        crate::preempt::enable();
        outcome
    }

    /// Hand the registration to the blocking pass. The count stays raised —
    /// see [`pass_block`].
    fn into_raw(self) -> RawTicket<'q> {
        self.0
    }
}

/// The blocking pass: commit the wait ticket **inside** the pass, after the
/// mailbox drain, and park on the same pass (spec §8.1's phase 2).
///
/// The commit cannot happen at the call site. A remote waker that claims a
/// task whose word already reads `Blocked` posts `Msg::Wake` to the task's home
/// CPU — which is this one — and the pass's own drain would consume that
/// message before the task is in `parked`, where `handle_wake` would find
/// nothing and drop it. Committing after the drain puts the claim on one side
/// or the other of it: an earlier claim finds `Committing` and posts nothing, so
/// the commit itself observes it and refuses to park; a later claim's message
/// arrives behind the drain and is handled by the next pass, which finds the
/// task parked.
///
/// Returns once the thread runs again, whatever ended the park — or not at
/// all, if a retire caught the thread mid-registration and the commit turned
/// the block into an exit.
pub fn pass_block(ticket: Ticket<'_>, deadline: Option<Nanos>) {
    // No `preempt::disable()` of its own: the ticket has held the count raised
    // since the registration published `Committing`, and that guard *is* this
    // pass's bracket. The window and the pass are one continuous preempt-off
    // region, which is the truth; taking a second level here would leave one
    // for the resuming context to discharge and one for nobody.
    let ticket = ticket.into_raw();
    crate::preempt::clear_need_resched();
    drain_irqs();
    let now = HW.now();
    let (action, registration) = with_cpu(|cpu| {
        let pass = SchedPass::begin(cpu, env(&PreemptOff(())), now);
        if let Some(current) = pass.cpu().running() {
            check_stack_canary(current.ext());
            current.ext().handle.publish(current.acct(), None);
        }
        match ticket.commit() {
            Commit::Parked(committed, registration) => (
                pass.dispose_block(committed, deadline).finish(),
                Some(registration),
            ),
            // A wake landed between registration and commit: do not park, do
            // not switch (spec §8.1). The pass still runs to its disposition,
            // because the quantum may have expired while we were deciding.
            Commit::AlreadyWoken => (pass.dispose_none().finish(), None),
            // A retire landed while this thread was deciding to park. Parking
            // is a safe point, so the kill is honoured here (spec §6.3, §7.6)
            // — the registration is already withdrawn, and this switch does
            // not return.
            Commit::Killed => (pass.dispose_exit().finish(), None),
        }
    });
    charge_cpu_time(now);
    with_cpu(|cpu| {
        if let Some(current) = cpu.running() {
            current.ext().handle.publish(current.acct(), Some(now));
        }
    });
    execute(action);
    crate::preempt::enable_no_resched();
    if let Some(registration) = registration {
        // Whatever ended the park, the node must leave the queue before this
        // thread can register anywhere else — otherwise a later `wake_one` on
        // the old queue would be satisfied by a waiter that is not waiting.
        registration.finish();
    }
}

/// Per-CPU busy time, for `sysinfo`. Derived from the same `now` the pass used,
/// so it cannot disagree with the task's own charge.
fn charge_cpu_time(now: Nanos) {
    let cpu = percpu::cpu_id() as usize;
    static LAST: [CpuTime; MAX_CPUS] = [const { CpuTime(AtomicU64::new(0)) }; MAX_CPUS];
    let last = LAST[cpu].0.swap(now.0, Ordering::Relaxed);
    if last != 0 && percpu::current_tid().is_some() {
        CPU_TIME_NS[cpu]
            .0
            .fetch_add(now.0.saturating_sub(last), Ordering::Relaxed);
    }
}

fn execute(action: Action<KernelPayload>) {
    match action {
        // SAFETY: the token came from `finish`, which built it from live
        // Box-backed task records; those records outlive the switch because the
        // only way to free one is `Hw::release`, which runs in a later pass.
        Action::Run(token) => unsafe { HW.switch(token) },
        Action::Resume => {}
        Action::Idle(token) => {
            // The final look, with interrupts off. A message that landed after
            // the pass's own check raised the doorbell, and its producer saw
            // SLEEPING and sent the IPI; taking that IPI here as an ordinary
            // interrupt and then halting is exactly B4, so re-check first.
            //
            // Not `Machine::irq_guard`: both exits must *set* IF — the halt
            // because `sti;hlt` is one atom, the stay-awake exit because panic
            // recovery reaches the idle loop with IF already 0.
            unsafe { asm!("cli", options(nomem, nostack)) };
            let cpu = CpuId(percpu::cpu_id());
            let awake = cpus().get(cpu).doorbell().kick_pending()
                || crate::preempt::need_resched()
                || crate::irq_ring::any_pending_self()
                || !with_cpu(|c| c.mailbox_is_empty())
                // The log ring owes the host bytes, so this CPU must not sleep
                // on them. `idle_loop` drains *before* the pass, and a line
                // logged after that drain — by `drain_irqs`, by a driver it
                // polls, by anything inside the pass — would otherwise wait for
                // the next wake. The LAPIC timer is one-shot, so on a genuinely
                // quiet machine there may not be one: the last thing the kernel
                // said before going silent is then not evidence of anything.
                //
                // It closes the window rather than narrowing it, by the same
                // argument §7.5 makes for the mailbox. A write that lands
                // between this load and the `hlt` came from an interrupt on this
                // CPU (which ends the halt through the STI shadow) or from
                // another CPU that is still running — and that CPU runs this
                // same check before it halts. The ring is one global buffer, so
                // whoever drains drains everything: the last CPU to sleep
                // flushes what all of them wrote.
                //
                // Declining rather than draining here is the point. A drain is
                // serial I/O, and `uart_write_bytes` spins on THRE; doing that
                // with interrupts off would put an unbounded wait exactly where
                // the machine is trying to go quiet. Returning sends this CPU
                // round the idle loop, which drains with interrupts on, one
                // bounded chunk per backend acquisition.
                //
                // The condition self-clears, which is what stops it becoming
                // the spin `i8042::service` documents for `any_pending_self`:
                // `drain_serial` loops until the ring reports empty, so one
                // trip round the idle loop always satisfies it. What would spin
                // a CPU is something on the *pass* path logging unconditionally
                // — which would also flood the ring, so it is already a bug.
                || crate::drivers::log_ring::has_pending()
                // A CPU with nothing left to run is the moment the i8042's
                // "the pin has never asserted" verdict stops being premature:
                // before it, silence only says the boot is still busy. A
                // wall-clock deadline cannot serve, because the driver is only
                // reached from inside a pass and the machine the verdict exists
                // for reaches `Boot: complete` and then has nothing to do — so
                // no pass would run to notice the deadline and the line would
                // never appear at all. Self-clearing on the same argument as the
                // ring above: the next pass emits the line and moves the state
                // on, so this costs one trip round the loop and never a spin.
                || crate::drivers::i8042::verdict_due()
                // And the same argument for the *file* sink, which has its own
                // cursor into the same ring: a machine with no serial port is
                // the one this matters on, and there the log ring drains into
                // nothing while `/boot/toyos/kernel.log` is the only surviving
                // copy. Self-clearing like the two above — `log_file::poll`
                // runs at the top of the loop and writes everything it is
                // owed, and the paths on which it cannot (a VFS lock a dead
                // thread still holds) turn the sink off after a bounded number
                // of tries, which clears this too.
                || crate::drivers::log_ring::file_has_pending()
                // A root-hub port whose connect state the driver has not
                // finished acting on. The connect edge that started it was the
                // last interrupt that controller has to give — a device sitting
                // still in a port produces nothing further — so no wake is
                // coming and the one-shot timer is armed for parked *tasks*,
                // which a driver's deferred work is not. Bounded and
                // self-clearing like the three above, but over a longer
                // interval: USB 2.0 §7.1.7.3's 100 ms of debounce, or the
                // transfer deadline behind a port that will not reset. It costs
                // an idle CPU the halt, never a pass — anything runnable is
                // still picked, because this decides only whether to sleep.
                || crate::drivers::xhci::port_work_pending();
            if awake {
                unsafe { asm!("sti", options(nomem, nostack)) };
                drop(token);
                return;
            }
            HW.idle_wait(token);
        }
    }
}

/// Consume this CPU's `irq_ring` records (spec §11 stage 2) and turn them into
/// wakes. Runs at the top of every pass, before the mailbox drain, so a wake
/// posted here is in the run queue by the time the pass picks.
fn drain_irqs() {
    // First in the function, so the stamp means "this CPU reached a pass" and
    // not "this CPU got all the way through one".
    #[cfg(feature = "heartbeat")]
    crate::heartbeat::note_pass();
    // xHCI (keyboard/mouse): the controller poll dispatches HID reports, which
    // wake the keyboard/mouse queues from inside the driver.
    crate::drivers::xhci::poll_if_pending();
    // The i8042's bytes are already in kernel memory when the IRQ returns;
    // this turns them into events and wakes.
    crate::drivers::i8042::service();
    // Ctrl+Alt+D. Here rather than at the keystroke, which is decoded under
    // whichever driver's guard produced it: this walks the scheduler and logs
    // a line per parked thread, and both drivers are done above.
    if crate::keyboard::take_dump_request() {
        super::dump::request();
    }
    // A CPU cannot read a sibling's `CpuSched`, so the dump reaches every CPU
    // by asking, and this is where each one answers.
    super::dump::serve_if_owed();

    if crate::irq_ring::take(crate::irq_ring::IrqSource::Net).is_some() {
        crate::net::wake_waiters();
        let watchers = crate::net::io_uring_watchers();
        if !watchers.is_empty() {
            crate::io_uring::complete_pending_for_event(
                &watchers,
                crate::io_uring::Source::Network,
            );
        }
    }
    if crate::irq_ring::take(crate::irq_ring::IrqSource::Audio).is_some() {
        // One wait queue for both backends: an over-wake costs a recheck, and a
        // second queue would have to be chosen by whichever driver bound —
        // which is a fact the parking side does not have.
        crate::audio::wake_waiters();
        for (watchers, source) in [
            (crate::audio::io_uring_watchers(), crate::io_uring::Source::Audio),
            (crate::drivers::hda::io_uring_watchers(), crate::io_uring::Source::Hda),
        ] {
            if !watchers.is_empty() {
                crate::io_uring::complete_pending_for_event(&watchers, source);
            }
        }
    }
}

/// Leave the current stack for this CPU's idle stack and never come back.
/// Boot and AP bring-up enter the scheduler here.
pub fn enter_idle_loop() -> ! {
    percpu::set_current_tid(None);
    percpu::set_current_pid(None);
    unsafe { percpu::set_kernel_stack(percpu::idle_stack_top()) };
    unsafe { crate::mm::paging::kernel_cr3().activate() };
    let sp = percpu::idle_stack_top();
    unsafe {
        asm!(
            "mov rsp, {sp}",
            // Terminate the frame chain, and leave the zero return-address
            // slot a `call` would have left. `idle_loop` is entered by `jmp`,
            // so its frame is the topmost on this stack and `rbp + 8` — where
            // `kernel_backtrace` reads the return address — was the unmapped
            // page above a 16 KiB idle stack. A fatal panic taken on an idle
            // CPU therefore faulted inside `crash_report` while printing its
            // own backtrace; that fault's report faulted the same way, and it
            // ended in a double fault with the panel carrying seven pages of
            // cascade and not one line of the reason. The one context a
            // machine-stopped panic is raised from was the one context that
            // could not say why.
            //
            // `push` also leaves `rsp` where the ABI expects it at a function
            // entry, which jumping to the raw top does not.
            "xor ebp, ebp",
            "push rbp",
            "jmp {func}",
            sp = in(reg) sp,
            func = in(reg) idle_loop as *const () as usize,
            options(noreturn),
        );
    }
}

extern "C" fn idle_loop() -> ! {
    loop {
        // The idle loop and not a pass: the state it stages is a CPU that never
        // reaches one.
        #[cfg(feature = "dump-deaf-cpu")]
        super::dump::deaf_window();
        // Here and not from a syscall: the panic handler recovers rather than
        // paints when a userland thread is current, and this context has none.
        #[cfg(feature = "metal-panic-probe")]
        if crate::drivers::panic_console::probe_due() {
            panic!("metal-panic-probe: a fatal report over a desktop that owns the screen");
        }
        crate::scheduler::log_health();
        crate::scheduler::reap_poisoned();
        drain_serial();
        // Immediately before the sink's poll, so the line it appends is flushed
        // by the very next statement rather than waiting a trip round the loop.
        #[cfg(feature = "heartbeat")]
        crate::heartbeat::poll();
        // After the serial drain and before the pass, for the same reason that
        // one is here: both are I/O off the critical path, and this is the one
        // context that provably holds none of the locks a filesystem needs.
        crate::log_file::poll();
        pass(Dispose::None);
    }
}

/// Flush buffered log output before sleeping. One chunk per backend
/// acquisition: the guard holds interrupts off for its lifetime, so a
/// full-ring drain under one guard would block them for up to 64 KiB of
/// serial I/O.
fn drain_serial() {
    loop {
        let mut backend = crate::drivers::serial::BackendGuard::lock();
        if crate::drivers::log_ring::drain_chunk_to_serial(&mut backend) == 0 {
            break;
        }
    }
}

/// The running task's rendezvous word, cloned so the caller can hold it across
/// its own block without borrowing the `CpuSched`.
pub fn current_shared() -> Option<Arc<KShared>> {
    try_with_cpu(|cpu| cpu.running().map(|t| t.shared().clone())).flatten()
}

pub fn current_cpu() -> CpuId {
    CpuId(percpu::cpu_id())
}

pub fn current_address_space() -> Option<PageTables> {
    try_with_cpu(|cpu| {
        cpu.running()
            .and_then(|t| t.ext().address_space.clone())
    })
    .flatten()
}

pub fn current_handle() -> Option<Arc<TaskHandle>> {
    try_with_cpu(|cpu| cpu.running().map(|t| t.ext().handle.clone())).flatten()
}

pub fn with_current_acct<R>(
    f: impl FnOnce(&toyos_sched::task::TaskAccounting) -> R,
) -> Option<R> {
    try_with_cpu(|cpu| cpu.running().map(|t| f(t.acct()))).flatten()
}

pub fn set_current_rt(permanent: bool) {
    with_cpu(|cpu| cpu.set_current_rt(permanent));
}

pub fn boost_current(until: Nanos) {
    with_cpu(|cpu| cpu.boost_current(until));
}

pub fn current_is_rt() -> bool {
    try_with_cpu(|cpu| cpu.running().is_some_and(|t| t.rt().is_rt())).unwrap_or(false)
}

pub fn ready_len() -> usize {
    try_with_cpu(|cpu| cpu.ready_len()).unwrap_or(0)
}

pub fn parked_len() -> usize {
    try_with_cpu(|cpu| cpu.parked().count()).unwrap_or(0)
}

/// The thread this CPU has loaded, if any.
pub fn running_id() -> Option<TaskId> {
    try_with_cpu(|cpu| cpu.running().map(|t| t.ext().id)).flatten()
}

/// One parked task, flattened for a reader outside the scheduler.
///
/// Flattened here because a `ParkedView` borrows the `CpuSched`, and nothing
/// outside this file may hold that borrow — that a CPU's state is reachable
/// only from that CPU is the property the whole core is built on.
pub struct ParkedInfo {
    pub id: TaskId,
    pub class: toyos_sched::task::WaitClass,
    pub deadline: Option<u64>,
    /// When the park began.
    pub since: u64,
    pub rt: bool,
}

/// Walk this CPU's parked tasks. `false` means a pass owns the state right
/// now, and a diagnostic does not wait for one.
pub fn for_each_parked(mut f: impl FnMut(ParkedInfo)) -> bool {
    try_with_cpu(|cpu| {
        for parked in cpu.parked() {
            f(ParkedInfo {
                id: parked.ext().id,
                class: parked.class(),
                deadline: parked.deadline().map(|n| n.0),
                since: parked.since().0,
                rt: parked.is_rt(),
            });
        }
    })
    .is_some()
}

/// Tail of the first switch into a fresh task, called by
/// `process_start`/`thread_start` before the first `iretq`.
///
/// There is no lock to release and no outgoing task to park — the pass that
/// switched here ended before the switch. All that is owed is the other half of
/// that pass's preempt-count bracket, which this context now inherits.
pub extern "sysv64" fn trampoline_entry() {
    crate::preempt::enable_no_resched();
    crate::arch::idt::kernel_exit_to_user_check();
}

const STACK_CANARY: u64 = 0xDEAD_BEEF_CAFE_BABE;

pub fn write_stack_canary(stack: &OwnedAlloc) {
    unsafe { *(stack.ptr() as *mut u64) = STACK_CANARY };
}

fn check_stack_canary(payload: &KernelPayload) {
    let canary = unsafe { *(payload.kernel_stack.ptr() as *const u64) };
    if canary != STACK_CANARY {
        panic!(
            "KERNEL STACK OVERFLOW: tid={} canary={:#x} expected={:#x}",
            payload.id.1, canary, STACK_CANARY
        );
    }
}

/// Callee-saved register save/restore. Unchanged from the old scheduler — the
/// switch was never the part that was wrong.
#[unsafe(naked)]
pub(crate) unsafe extern "C" fn context_switch(old_rsp: *mut u64, new_rsp: u64) {
    naked_asm!(
        "pushfq",
        "push rbp",
        "push rbx",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "mov [rdi], rsp",
        "mov rsp, rsi",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbx",
        "pop rbp",
        "popfq",
        "ret",
    );
}
