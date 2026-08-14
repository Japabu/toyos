//! The kernel-facing scheduler API (spec §4's `kernel/src/sched/mod.rs`).
//!
//! The scheduler itself is `toyos-sched`, driven by `kernel/src/sched/`. This
//! file is *only* a surface: no decision, no state transition and no
//! ordering-sensitive step happens here.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

use hashbrown::HashMap;
use toyos_sched::fair::{ShareState, QUANTUM_NS};
use toyos_sched::hw::{Machine, Nanos};
use toyos_sched::task::{WakeCause, WakeReason};

use crate::arch::percpu;
use crate::hw::HW;
use crate::pipe::PipeId;
use crate::process::{self, Pid, Tid};
use crate::sched::driver::{self, cpus, preempt_off, Dispose, NewTask};
use crate::sched::payload::{KShare, KWaitQueue, KernelLock, ThreadSched};
use crate::sched::waitqs;
use crate::sync::Lock;
use crate::DirectMap;

pub use crate::sched::driver::{
    current_address_space, enter_idle_loop, in_pass as in_schedule_self, total_cpu_ns,
    write_stack_canary, Ticket,
};
pub use crate::sched::MAX_CPUS;

/// Spec §6.4's lock-across-switch tripwire.
///
/// A `sync::Lock` guard raises the preempt count and keeps it raised until it
/// drops, so a count *above* what the calling context is entitled to means a
/// spinlock is held. Reaching a switching scheduler entry that way parks the
/// lock on a stack nothing will return to, and every other CPU that takes it
/// then spins into `Lock::lock`'s 500M-spin DEADLOCK panic — which names the
/// victim and never the culprit. This names the culprit, at its own call site.
///
/// `#[track_caller]` all the way down is what makes the message point outside
/// this file; without it every trip reports the same three lines.
#[track_caller]
fn assert_baseline(baseline: u32) {
    let depth = crate::preempt::count();
    assert!(
        depth == baseline,
        "scheduler entered while a lock is held: preempt depth {depth}, baseline {baseline}",
    );
}

/// The depth an *unnested* trap handler runs at: one level, raised by the entry
/// asm (`arch/syscall.rs`, `common_entry`) and lowered on the way out.
///
/// Each entry raises its own level, so a fault taken inside a syscall runs at
/// two — routine, not hypothetical (a demand-paging fault on a user page the
/// handler touches). No asserting entry is reachable from there today: every
/// kernel-mode fault funnels to `schedule_no_return`, which deliberately does
/// not assert. The first demand-paging path that parks instead of spinning, or
/// any decision to kill a kernel-faulting process through `process::exit`,
/// breaks that and trips this on a nested trap holding no lock at all. The
/// check establishes `depth != baseline`; the message names the cause that
/// motivates it, and a nested trap is the other way to get there.
const BASELINE_TRAP: u32 = 1;

/// The depth the deferred-preempt poll runs at. Zero, and not `BASELINE_TRAP`,
/// because all three routes into it are *past* the entry level: the Ring 3
/// timer stub (`arch/idt/timer.rs`) never raises one, `kernel_exit_to_user_check`
/// (`arch/idt/mod.rs`) runs after the `lock sub`, and `preempt::enable`'s slow
/// path only calls in at zero. The idle loop reaches it through the third —
/// `reap_poisoned`'s `PROCESS_TABLE` guard drop — not as a route of its own.
const BASELINE_IRQ_EXIT: u32 = 0;

/// Process-scoped thread identity. Tids are per-process, so the scheduler
/// needs the pair to name a thread system-wide.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TaskId(pub Pid, pub Tid);

impl TaskId {
    pub fn pack(self) -> u64 {
        self.1.raw() as u64 | (self.0.raw() as u64) << 32
    }
    pub fn unpack(v: u64) -> Self {
        Self(Pid::from_raw((v >> 32) as u32), Tid::from_raw(v as u32))
    }
}

impl core::fmt::Display for TaskId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}:{}", self.0, self.1)
    }
}

/// Pid → share. Touched at spawn and at process teardown only; the *charge*
/// path reaches the share through the task that owns it, which is what retires
/// the old scheduler's one-hot-lock-per-charge (spec §9.1).
static SHARES: Lock<Option<HashMap<Pid, Arc<KShare>>>> = Lock::new(None);

pub fn init() {
    *SHARES.lock() = Some(HashMap::new());
    driver::init();
}

/// The share a new task of `pid` joins. Created `NonRunnable { lag: 0 }` so
/// that the adopting CPU's `enter_runnable` produces exactly the old
/// `new_runnable(frontier)` state: vruntime at the frontier, refcount one.
fn share_for(pid: Pid) -> Arc<KShare> {
    let mut guard = SHARES.lock();
    let map = guard.as_mut().expect("scheduler not initialized");
    map.entry(pid)
        .or_insert_with(|| {
            Arc::new(KShare::new(KernelLock::new(ShareState::NonRunnable {
                lag: 0,
            })))
        })
        .clone()
}

fn share_of(pid: Pid) -> Option<Arc<KShare>> {
    SHARES.lock().as_ref()?.get(&pid).cloned()
}

/// The process is gone from the table. Live tasks keep their `Arc` alive, so
/// a thread still finishing its exit path can still be charged.
pub fn remove_vruntime(pid: Pid) {
    if let Some(map) = SHARES.lock().as_mut() {
        map.remove(&pid);
    }
}

pub fn process_vruntime(pid: Pid) -> u64 {
    share_of(pid).map_or(0, |s| s.vruntime(driver::frontier()))
}

pub fn process_lag(pid: Pid) -> i64 {
    share_of(pid).map_or(0, |s| s.lag())
}

pub fn global_min_vruntime() -> u64 {
    driver::frontier().get()
}

/// Build and place a new task. The caller supplies everything but the share.
pub fn enqueue_new(
    id: TaskId,
    kernel_stack: crate::process::OwnedAlloc,
    entry_rsp: u64,
    address_space: Option<crate::process::PageTables>,
    fs_base: u64,
) -> ThreadSched {
    driver::spawn(NewTask {
        id,
        kernel_stack,
        entry_rsp,
        address_space,
        fs_base,
        share: share_for(id.0),
    })
}

/// Phase 1 of the wait handshake: register the running thread on `queue`.
/// The caller must then re-check its condition and either cancel the ticket or
/// block on it — registering *before* the re-check is what closes the
/// check-then-block window.
///
/// The ticket holds preemption off until it is consumed; the re-check may take
/// whatever locks it needs, and the deferred request is served by the block or
/// by the cancel. See [`Ticket`].
#[must_use = "a wait ticket must be blocked on or cancelled"]
#[track_caller]
pub fn prepare_wait(queue: &KWaitQueue) -> Ticket<'_> {
    assert_baseline(BASELINE_TRAP);
    Ticket::register(queue)
}

/// Phase 2: park the running thread on the queue it registered with.
///
/// Taking the ticket by value is the whole point: a park that reaches the
/// machine without a registration behind it is the lost-wake window, and there
/// is no other way to construct one. `deadline = 0` means no timeout.
#[track_caller]
pub fn block_on(ticket: Ticket<'_>, deadline: u64) {
    // One level above the trap baseline: the ticket has held the registration
    // window's own level since `prepare_wait`, and `pass_block` inherits it.
    assert_baseline(BASELINE_TRAP + 1);
    driver::pass_block(ticket, (deadline > 0).then(|| Nanos(deadline)));
}

/// Register, re-check, park — for a site whose condition is exactly `ready` —
/// and **hold the wait until that condition is true**.
///
/// A return from a park is not evidence that this queue is what woke the
/// thread. A task is woken *by name* as well as by queue: every child thread's
/// exit posts `wake_task` to its process's main thread (`process::thread_exit`),
/// panic recovery wakes a joiner, a futex bucket is shared by every word that
/// hashes into it, and a deadline fires on the task's own CPU. Checking once
/// and returning made every caller's answer depend on which of those arrived
/// first — `sys_process_wait` read an exit code that had not been published
/// yet and killed the kernel from a plain `Child::wait()`. So the predicate is
/// re-checked after every wake and the thread re-parks until it holds, which
/// is what `sched::waitqs` already documents every blocking site as doing and
/// what spec §2's invariant 10 requires of one. A site that parks with
/// `prepare_wait`/`block_on` directly owns that loop itself — `sys_nanosleep`
/// is the one that does not
/// (`specs/issues/kernel/nanosleep-ends-early-when-a-sibling-thread-exits.md`).
///
/// Looping does not weaken spec §2's no-lost-wake invariant, because each trip
/// is the whole two-phase handshake again: the re-registration happens *before*
/// the re-check, so a wake landing in between claims the new ticket and the
/// commit refuses to park.
///
/// `deadline` still bounds the wait. It is absolute, so a re-park carries the
/// same one and the wait ends no later than it was going to; an expiry returns
/// with the condition false, which is what the one timed caller
/// (`sys_read`'s console) needs — it re-derives its answer from the object
/// rather than from this return, inside a loop of its own.
///
/// A killed task never comes back round: a retire that lands while it is
/// deciding to park turns the block into an exit (`Commit::Killed`, spec §6.3),
/// and one that lands while it is parked releases it where it lies (§2.7). No
/// path here can hold a dying thread in a wait.
#[track_caller]
pub fn wait_until(queue: &KWaitQueue, deadline: u64, ready: impl Fn() -> bool) {
    loop {
        let ticket = prepare_wait(queue);
        if ready() {
            ticket.cancel();
            return;
        }
        block_on(ticket, deadline);
        if deadline != 0 && crate::hw::now_ns() >= deadline {
            return;
        }
    }
}

/// A parking lot for waits woken by name rather than by condition — waitpid,
/// thread_join, nanosleep. Never woken as a queue; see `sched::waitqs`.
pub fn park_lot() -> &'static KWaitQueue {
    waitqs::park_lot(percpu::current_tid().map_or(0, |t| t.raw() as u64))
}

#[track_caller]
pub fn yield_now() {
    assert_baseline(BASELINE_TRAP);
    driver::pass(Dispose::Yield);
}

/// Unified preempt entry — the Ring 3 timer path, `kernel_exit_to_user_check`
/// and the `preempt::enable` slow path all funnel through here. The pass
/// itself decides whether the running thread keeps the CPU (quantum expiry or
/// an RT task in the band); this only asks it to look.
#[track_caller]
pub fn do_preempt() {
    if in_schedule_self() {
        return;
    }
    assert_baseline(BASELINE_IRQ_EXIT);
    crate::preempt::clear_need_resched();
    if percpu::current_tid().is_none() {
        // No thread on this CPU: either the idle loop, which passes every
        // iteration anyway, or boot, which has no `CpuSched` yet — an ISR that
        // raised the request during device init would otherwise reach the
        // machine before it exists. The request is moot, not deferred.
        return;
    }
    crate::trace::trace(crate::trace::Kind::Preempt, 0);
    driver::pass(Dispose::None);
}

#[track_caller]
pub fn exit_current(code: i32) -> ! {
    assert_baseline(BASELINE_TRAP);
    {
        let mut guard = process::PROCESS_TABLE.lock();
        let table = guard.as_mut().unwrap();
        let tid = percpu::current_tid().unwrap();
        let pid = percpu::current_pid().unwrap();
        process::mark_thread_zombie(table, pid, tid, code);
    }
    driver::pass(Dispose::Exit);
    unreachable!("exit_current: returned from the exit pass");
}

/// Wake one specific thread — waitpid, thread_join, panic-recovery notify.
/// The same claim CAS every other wake goes through, without a queue: the
/// waiter's own `Registration` takes its node out of the parking lot when it
/// runs again (spec §8.2).
///
/// No §6.4 baseline assert here or on any other wake path: a wake posts a
/// message and never switches, and waking from *inside* a lock is the protocol
/// rather than a violation of it (§8.1's claim-and-post happens under the waitq
/// leaf lock, and `KernelLock` is documented as a legal mailbox producer for
/// exactly that reason).
pub fn wake_task(id: TaskId) {
    let Some(sched) = process::thread_sched(id.0, id.1) else {
        return;
    };
    wake_sched(&sched);
}

pub fn wake_sched(sched: &ThreadSched) {
    preempt_off(|p| {
        toyos_sched::waitq::wake_direct(
            &sched.shared,
            WakeCause::new(WakeReason::Woken),
            cpus(),
            &HW,
            p,
        )
    });
}

/// Wake pipe readers, lending each an RT window if the writer holds one
/// (spec §8.5). The pipe is also marked, so a reader that was runnable rather
/// than blocked at write time takes the window at its own consume point.
pub fn wake_pipe_readers(pipe_id: PipeId) {
    let Some(queue) = crate::pipe::readers_queue(pipe_id) else {
        return;
    };
    if driver::current_is_rt() {
        crate::pipe::set_rt_boost_pending(pipe_id);
        waitqs::wake_all_boosted(&queue, boost_window());
    } else {
        waitqs::wake_all(&queue);
    }
}

pub fn wake_pipe_writers(pipe_id: PipeId) {
    if let Some(queue) = crate::pipe::writers_queue(pipe_id) {
        waitqs::wake_all(&queue);
    }
}

/// How long a lent RT priority lasts: a wall-clock bound on time *held*
/// (spec §8.5, invariant I9), one quantum wide.
pub fn boost_window() -> Nanos {
    HW.now().after(QUANTUM_NS)
}

/// Grant the running thread the window its producer left on a pipe.
pub fn boost_current_rt_inherited() {
    driver::boost_current(boost_window());
}

/// `SYS_RT_ENTER`. Gated at the dispatch site on `Rights::RT`, not here — this
/// must stay callable from kernel init. That right is spec §9.4's privilege
/// gate, and it is endowed per manifest rather than won: what gated the band
/// before was a sound-device claim, which is not a privilege at all.
pub fn set_current_rt(enable: bool) {
    driver::set_current_rt(enable);
}

/// Block on a futex word unless it already changed. Returns whether it parked.
///
/// Registering before reading the word is the whole protocol: a `futex_wake`
/// that runs after the registration either claims the ticket or finds the
/// waiter parked, and one that ran before it stored the new value before the
/// registration — so the read below sees it.
#[track_caller]
pub fn futex_wait(phys_addr: DirectMap, expected: u32, deadline: u64) -> bool {
    let queue = waitqs::futex(phys_addr);
    let ticket = prepare_wait(queue);
    if unsafe { *phys_addr.as_ptr::<u32>() } != expected {
        ticket.cancel();
        return false;
    }
    block_on(ticket, deadline);
    true
}

/// Wake up to `count` futex waiters. Buckets are shared, so this can wake a
/// waiter of a different word — harmless, every waiter re-checks its own.
pub fn futex_wake(phys_addr: DirectMap, count: usize) -> u64 {
    waitqs::wake_n(waitqs::futex(phys_addr), count) as u64
}

/// Retire a thread and wait until its record is gone.
///
/// The retire itself is one message (spec §7.6): the sticky kill bit plus
/// `Msg::Retire` to the CPU the state word names, and whichever CPU ends up
/// owning the task reaps it — parked, queued, in transit, or at the next safe
/// point if it is running. Nothing scans anything and nobody spins.
///
/// The *wait* is what the callers need and why this is not fire-and-forget:
/// process teardown frees memory the dead thread's page tables still map, so
/// it may not run until that thread's payload — kernel stack and address-space
/// reference — is dropped. That happens in `Hw::release`, which announces
/// itself here. Waiting for the state word to read `Dead` would be too weak:
/// `Dead` is published by the reaping *transition*, one pass before the
/// release, while the dying CPU still stands on that thread's kernel stack.
///
/// The short block deadline is a liveness backstop, not a poll: the wake is a
/// message like any other, and a lost one must fail loudly rather than hang.
#[track_caller]
pub fn retire_task(sched: &ThreadSched) {
    // Also on the early-return path below, where no park happens and the two
    // asserts inside the wait would never run.
    assert_baseline(BASELINE_TRAP);
    if let (Some(pid), Some(tid)) = (percpu::current_pid(), percpu::current_tid()) {
        if let Some(handle) = driver::current_shared() {
            assert!(
                !Arc::ptr_eq(&handle, &sched.shared),
                "retire_task: cannot retire self ({})",
                TaskId(pid, tid),
            );
        }
    }
    if sched.handle.released() {
        return;
    }
    preempt_off(|p| {
        toyos_sched::retire::begin(&sched.shared).post(cpus(), &HW, p);
    });
    const RECHECK_NS: u64 = 50_000_000;
    let give_up = crate::hw::now_ns() + 1_000_000_000;
    while !sched.handle.released() {
        if crate::hw::now_ns() > give_up {
            panic!(
                "retire_task: task not released after 1s: {:?}",
                sched.shared.state()
            );
        }
        let ticket = prepare_wait(sched.handle.released_wait());
        if sched.handle.released() {
            ticket.cancel();
            return;
        }
        block_on(ticket, crate::hw::now_ns() + RECHECK_NS);
    }
}

/// Per-CPU hand-off slot for a thread that died in panic recovery. The panic
/// path may hold any lock, so it may do nothing but store here; the idle loop
/// is the thread's only cleanup site.
static POISONED: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(u64::MAX) }; MAX_CPUS];

pub fn poison_tid(id: TaskId) {
    let cpu = percpu::cpu_id() as usize;
    let Some(slot) = POISONED.get(cpu) else {
        crate::log!("poison_tid: cpu {cpu} >= MAX_CPUS — {id} will never be reaped");
        return;
    };
    let prev = slot.swap(id.pack(), Ordering::Release);
    if prev != u64::MAX {
        crate::log!(
            "poison_tid: cpu {cpu} slot still held {} — its waiter is stranded",
            TaskId::unpack(prev)
        );
    }
}

/// Zombify threads that died in panic recovery and wake whoever was joining
/// them. Called from the idle loop, which is the one context that provably
/// holds none of the locks the panicking thread may have been holding.
pub(crate) fn reap_poisoned() {
    let mut wakes: [Option<process::PoisonWake>; MAX_CPUS] = [const { None }; MAX_CPUS];
    // Both are dropped after the guard: an entry's drop reaches
    // `remove_vruntime`, and a process whose teardown never ran still holds its
    // whole `ProcessData` here.
    let reaped;
    {
        let mut guard = process::PROCESS_TABLE.lock();
        let table = guard.as_mut().unwrap();
        reaped = process::reap_finished(table, unsafe { process::IdleProof::new_unchecked() });
        for (slot, wake) in POISONED.iter().zip(wakes.iter_mut()) {
            let raw = slot.load(Ordering::Relaxed);
            if raw == u64::MAX {
                continue;
            }
            let id = TaskId::unpack(raw);
            *wake = process::zombify_poisoned(table, id.0, id.1);
            slot.store(u64::MAX, Ordering::Relaxed);
        }
    }
    drop(reaped);
    for wake in wakes.into_iter().flatten() {
        match wake {
            process::PoisonWake::Joiner(pid, tid) => wake_task(TaskId(pid, tid)),
            // The code a killed process gets: nobody asked for this exit, and
            // the accounting the teardown would have taken was never taken.
            process::PoisonWake::Process(object) => {
                let stats = toyos_abi::syscall::ProcessStats {
                    pid: object.pid().raw(),
                    ..Default::default()
                };
                object.publish_exit(crate::object::process::Exit { code: -1, stats })
            }
        }
    }
}

/// The panic path's exit: the faulted thread's context is unusable, so it dies
/// where it stands. Its record becomes this CPU's zombie and is released by the
/// next pass, which by then runs on another stack.
///
/// The one switching entry with no §6.4 baseline assert, deliberately: a
/// panicking thread may hold any lock — that is the situation, not a bug to
/// trip over — and measurement finds this entry at both baselines. Asserting
/// here would turn every panic-with-a-lock into a double panic and lose the
/// report. The dying context's depth leaves with it, since `Hw::switch` loads
/// the incoming context's own.
pub fn schedule_no_return() -> ! {
    if in_schedule_self() {
        crate::log!("schedule_no_return: panicked inside a pass, cannot rejoin");
        crate::arch::apic::halt_all_cpus();
    }
    if percpu::current_tid().is_none() {
        enter_idle_loop();
    }
    driver::pass(Dispose::Exit);
    unreachable!("schedule_no_return: returned from the exit pass");
}

/// Cumulative CPU time for a thread, published by its owning CPU at each end of
/// a pass (see `TaskHandle`). A running thread's live slice is added by the
/// reader, so the number does not stand still between passes.
pub fn task_cpu_ns(sched: &ThreadSched) -> u64 {
    sched.handle.cpu_ns()
}

pub fn task_sched_state(sched: &ThreadSched) -> u8 {
    sched.sched_state()
}

/// Flush the running thread's blocked/runqueue counters into process
/// accounting. Reads the live task's own record — a local access, which is the
/// only kind a `!Sync` `CpuSched` admits.
pub fn flush_current_stats(acct: &mut process::ProcessAccounting) {
    driver::with_current_acct(|a| crate::sched::payload::merge_accounting(a, acct));
}

/// How often an idle CPU may say what it is holding, and how often the machine
/// may say what it has allocated.
///
/// One number for both because they are one kind of thing: a periodic snapshot
/// of occupancy, taken from the idle loop, by a machine whose only channel may
/// be a log file on the stick it booted from. The occupancy of the run queues
/// and the occupancy of the page pools are read together or not at all.
const SNAPSHOT_INTERVAL_NS: u64 = 10_000_000_000;

/// When each CPU may next print its own line. Per CPU rather than global: which
/// CPUs reach idle is most of what the line says, and one global deadline would
/// let whichever CPU won the race speak for all of them.
static NEXT_HEALTH: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

/// A snapshot of this CPU's run queues, at most once per
/// [`SNAPSHOT_INTERVAL_NS`], plus the machine's page pools on the same cadence.
///
/// Called from the idle loop on every trip, and the cadence is a wall clock
/// because a trip is not a unit of time. It used to be one line per 1000 trips
/// of a counter shared by every CPU, and a CPU that declines to sleep — the log
/// ring owes bytes, an xHCI port is inside its debounce — goes round that loop
/// at memory speed: measured on the USB profiles, bursts of one line every 3–4
/// ms from each of two CPUs, about 320 lines a second, 292 of them in one run
/// of the xHCI family. Worse than the volume is the feedback: every line is
/// bytes the ring owes, and bytes the ring owes is one of the conditions that
/// stops the CPU sleeping. The line was printing because the machine was awake
/// and keeping it awake by printing.
///
/// The line is kept rather than deleted because `parked` is not readable
/// anywhere else without a message round trip (`dump_blocked` reaches only the
/// calling CPU, and only on a keystroke), and on the machine with no serial
/// port an occasional occupancy line in `kernel.log` is the only account of the
/// scheduler there is. What it must not be read as is a heartbeat: it comes
/// from a CPU passing through idle, so a quiet machine prints nothing and a gap
/// is not evidence.
pub fn log_health() {
    let now = crate::hw::now_ns();
    let cpu = percpu::cpu_id();
    let Some(next_health) = NEXT_HEALTH.get(cpu as usize) else { return };
    if now >= next_health.load(Ordering::Relaxed) {
        next_health.store(now + SNAPSHOT_INTERVAL_NS, Ordering::Relaxed);
        let ready = driver::ready_len() + usize::from(percpu::current_tid().is_some());
        let parked = driver::parked_len();
        crate::log!(
            "sched: cpu={} ready={} parked={} current={:?}",
            cpu,
            ready,
            parked,
            percpu::current_tid()
        );
    }

    static NEXT_PMM_DUMP: AtomicU64 = AtomicU64::new(0);
    let next = NEXT_PMM_DUMP.load(Ordering::Relaxed);
    if next == 0 {
        NEXT_PMM_DUMP.store(now + SNAPSHOT_INTERVAL_NS, Ordering::Relaxed);
    } else if now >= next
        && NEXT_PMM_DUMP
            .compare_exchange(
                next,
                now + SNAPSHOT_INTERVAL_NS,
                Ordering::Relaxed,
                Ordering::Relaxed,
            )
            .is_ok()
    {
        crate::mm::pmm::dump_stats();
    }
}

