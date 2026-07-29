//! The kernel-facing scheduler API (spec §4's `kernel/src/sched/mod.rs`).
//!
//! Migration stage 7a: the kernel drives `toyos-sched`. Everything that used to
//! live in this file — the cross-CPU run queues, the global blocked pool, the
//! kill set, the transit counter, `handle_outgoing`'s post-switch parking — has
//! no successor, because the machine underneath now makes each of those bugs
//! unrepresentable rather than guarded. What is left here is the surface the
//! rest of the kernel calls, and it is *only* a surface: no decision, no state
//! transition and no ordering-sensitive step happens in this file.
//!
//! What 7a deliberately does not do: `StealRequest` and balance stay switched
//! off (`Env::steal = false`), so placement is spawn-time and wake-time push
//! only. That is stage 7b.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

use hashbrown::HashMap;
use toyos_sched::fair::{ShareState, QUANTUM_NS};
use toyos_sched::hw::{Machine, Nanos};
use toyos_sched::task::{TaskState, WakeCause, WakeReason};

use crate::arch::percpu;
use crate::hw::HW;
use crate::io_uring::RingId;
use crate::listener::ListenerId;
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

// ---------------------------------------------------------------------------
// What a source's readiness means — io_uring's poll key
// ---------------------------------------------------------------------------

/// What an io_uring `POLL_ADD` is registered on.
///
/// No longer a scheduler concept: the scheduler knows only tasks, tickets and
/// causes (spec §8.1). This is io_uring's key for "which rings care about this
/// object", and it names the same objects the wait queues belong to.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum EventSource {
    Keyboard,
    Mouse,
    Network,
    Listener(ListenerId),
    PipeReadable(PipeId),
    PipeWritable(PipeId),
    Audio,
    Futex(DirectMap),
    IoUring(RingId),
}

/// Is the source ready right now? Used by io_uring's poll recheck and by
/// blocking sites whose re-check is exactly this.
pub fn source_ready(event: &EventSource) -> bool {
    match event {
        EventSource::Keyboard => crate::keyboard::has_data(),
        EventSource::Mouse => crate::mouse::has_data(),
        EventSource::Network => crate::net::has_packet(),
        EventSource::Listener(id) => crate::listener::has_pending_by_id(*id),
        EventSource::PipeReadable(id) => crate::pipe::has_data(*id),
        EventSource::PipeWritable(id) => crate::pipe::has_space(*id),
        EventSource::Audio => crate::audio::has_pending(),
        EventSource::Futex(_) => false,
        EventSource::IoUring(ring) => crate::io_uring::has_completions(*ring),
    }
}

// ---------------------------------------------------------------------------
// Per-process fair share
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Spawn
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Blocking
// ---------------------------------------------------------------------------

/// Phase 1 of the wait handshake: register the running thread on `queue`.
/// The caller must then re-check its condition and either cancel the ticket or
/// block on it — registering *before* the re-check is what closes the
/// check-then-block window.
///
/// The ticket holds preemption off until it is consumed; the re-check may take
/// whatever locks it needs, and the deferred request is served by the block or
/// by the cancel. See [`Ticket`].
#[must_use = "a wait ticket must be blocked on or cancelled"]
pub fn prepare_wait(queue: &KWaitQueue) -> Ticket<'_> {
    Ticket::register(queue)
}

/// Phase 2: park the running thread on the queue it registered with.
///
/// Taking the ticket by value is the whole point: a park that reaches the
/// machine without a registration behind it is the lost-wake window, and there
/// is no other way to construct one. `deadline = 0` means no timeout.
pub fn block_on(ticket: Ticket<'_>, deadline: u64) {
    driver::pass_block(ticket, (deadline > 0).then(|| Nanos(deadline)));
}

/// Register, re-check, park — for a site whose condition is exactly `ready`.
pub fn wait_until(queue: &KWaitQueue, deadline: u64, ready: impl Fn() -> bool) {
    let ticket = prepare_wait(queue);
    if ready() {
        ticket.cancel();
    } else {
        block_on(ticket, deadline);
    }
}

/// A parking lot for waits woken by name rather than by condition — waitpid,
/// thread_join, nanosleep. Never woken as a queue; see `sched::waitqs`.
pub fn park_lot() -> &'static KWaitQueue {
    waitqs::park_lot(percpu::current_tid().map_or(0, |t| t.raw() as u64))
}

pub fn yield_now() {
    driver::pass(Dispose::Yield);
}

/// Unified preempt entry — the Ring 3 timer path, `kernel_exit_to_user_check`
/// and the `preempt::enable` slow path all funnel through here. The pass
/// itself decides whether the running thread keeps the CPU (quantum expiry or
/// an RT task in the band); this only asks it to look.
pub fn do_preempt() {
    if in_schedule_self() {
        return;
    }
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

pub fn exit_current(code: i32) -> ! {
    {
        let mut guard = process::PROCESS_TABLE.lock();
        let table = guard.as_mut().unwrap();
        let tid = percpu::current_tid().unwrap();
        let pid = percpu::current_pid().unwrap();
        process::zombify_tid(table, pid, tid, code);
    }
    driver::pass(Dispose::Exit);
    unreachable!("exit_current: returned from the exit pass");
}

// ---------------------------------------------------------------------------
// Wakes
// ---------------------------------------------------------------------------

/// Wake one specific thread — waitpid, thread_join, panic-recovery notify.
/// The same claim CAS every other wake goes through, without a queue: the
/// waiter's own `Registration` takes its node out of the parking lot when it
/// runs again (spec §8.2).
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

/// How long a lent RT priority lasts. The old code cleared the borrowed
/// priority at the boosted thread's next deschedule, whatever that was; the
/// core makes it a wall-clock bound instead (spec §8.5, invariant I9), and one
/// quantum is the closest honest translation.
pub fn boost_window() -> Nanos {
    HW.now().after(QUANTUM_NS)
}

/// Grant the running thread the window its producer left on a pipe.
pub fn boost_current_rt_inherited() {
    driver::boost_current(boost_window());
}

/// `SYS_SET_RT_PRIORITY`. The privilege gate is spec §9.4's, and is still
/// missing — see the known issue in CLAUDE.md.
pub fn set_current_rt(enable: bool) {
    driver::set_current_rt(enable);
}

// ---------------------------------------------------------------------------
// Futex
// ---------------------------------------------------------------------------

/// Block on a futex word unless it already changed. Returns whether it parked.
///
/// Registering before reading the word is the whole protocol: a `futex_wake`
/// that runs after the registration either claims the ticket or finds the
/// waiter parked, and one that ran before it stored the new value before the
/// registration — so the read below sees it.
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

// ---------------------------------------------------------------------------
// Retire
// ---------------------------------------------------------------------------

/// Remove a thread from the scheduler with proof of absence: when this returns,
/// `id` is not queued, not parked and not running, and it can never reappear —
/// the state word says `Dead`, and the sticky kill bit means any CPU that ends
/// up owning it reaps it on arrival (spec §7.6).
///
/// Stage 7a keeps this synchronous because process teardown frees memory the
/// target's page tables still map. Stage 7b makes it a bare message.
pub fn retire_task(sched: &ThreadSched) {
    if let (Some(pid), Some(tid)) = (percpu::current_pid(), percpu::current_tid()) {
        if let Some(handle) = driver::current_shared() {
            assert!(
                !Arc::ptr_eq(&handle, &sched.shared),
                "retire_task: cannot retire self ({})",
                TaskId(pid, tid),
            );
        }
    }
    if sched.shared.state() == TaskState::Dead {
        return;
    }
    preempt_off(|p| {
        toyos_sched::retire::begin(&sched.shared).post(cpus(), &HW, p);
    });
    let deadline = crate::hw::now_ns() + 1_000_000_000;
    while sched.shared.state() != TaskState::Dead {
        if crate::hw::now_ns() > deadline {
            panic!("retire_task: task still alive after 1s: {:?}", sched.shared.state());
        }
        yield_now();
    }
}

// ---------------------------------------------------------------------------
// Panic recovery
// ---------------------------------------------------------------------------

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
    let mut wakes = [None; MAX_CPUS];
    {
        let mut guard = process::PROCESS_TABLE.lock();
        let table = guard.as_mut().unwrap();
        process::collect_orphan_zombies(table, unsafe { process::IdleProof::new_unchecked() });
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
    for (pid, tid) in wakes.into_iter().flatten() {
        wake_task(TaskId(pid, tid));
    }
}

/// The panic path's exit: the faulted thread's context is unusable, so it dies
/// where it stands. Its record becomes this CPU's zombie and is released by the
/// next pass, which by then runs on another stack.
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

// ---------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------

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

pub fn log_health() {
    let ready = driver::ready_len() + usize::from(percpu::current_tid().is_some());
    let parked = driver::parked_len();
    crate::log!(
        "sched: cpu={} ready={} parked={} current={:?}",
        percpu::cpu_id(),
        ready,
        parked,
        percpu::current_tid()
    );

    static NEXT_PMM_DUMP: AtomicU64 = AtomicU64::new(0);
    const PMM_DUMP_INTERVAL_NS: u64 = 10_000_000_000;
    let now = crate::hw::now_ns();
    let next = NEXT_PMM_DUMP.load(Ordering::Relaxed);
    if next == 0 {
        NEXT_PMM_DUMP.store(now + PMM_DUMP_INTERVAL_NS, Ordering::Relaxed);
    } else if now >= next
        && NEXT_PMM_DUMP
            .compare_exchange(
                next,
                now + PMM_DUMP_INTERVAL_NS,
                Ordering::Relaxed,
                Ordering::Relaxed,
            )
            .is_ok()
    {
        crate::mm::pmm::dump_stats();
    }
}

/// Dump this CPU's parked threads.
///
/// Only this CPU's: a `CpuSched` is `!Sync`, so there is no way to walk a
/// sibling's parked map, and inventing one would be inventing the shared state
/// the whole design removes. What a cross-CPU view costs is a message round
/// trip; whether that is worth building is a diagnostics question, not a
/// scheduler one.
pub fn dump_blocked() {
    crate::log!(
        "=== PARKED THREADS on cpu {} ({}) ===",
        percpu::cpu_id(),
        driver::parked_len()
    );
    driver::for_each_parked(|key, deadline, class| {
        crate::log!("  task={:?} class={:?} deadline={:?}", key, class, deadline);
    });
    crate::log!("=== END PARKED ===");
}

