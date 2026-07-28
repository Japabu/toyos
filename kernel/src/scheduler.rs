use core::arch::{asm, naked_asm};
use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use hashbrown::{HashMap, HashSet};
use toyos_sched::fair::{Frontier, ShareState, QUANTUM_NS};
use toyos_sched::hw::{CpuId, Kicker, Machine, Nanos, TraceEvent, TraceKind};

use crate::arch::{cpu, percpu};
use crate::hw::HW;
use crate::io_uring::RingId;
use crate::listener::ListenerId;
use crate::pipe::PipeId;
use crate::process::{self, IdleProof, OwnedAlloc, PageTables, Pid, Tid, KERNEL_STACK_SIZE};
use crate::sync::Lock;
use crate::waitq::{WaitQueue, WaitTicket};
use crate::DirectMap;

pub const MAX_CPUS: usize = 8;

/// Compile-time verbose scheduler logging. When `true`, every cpus[N] lock
/// acquire/release, every steal probe, every context-switch boundary is
/// printed to serial. Useful for debugging cross-CPU lock state. Off by
/// default — log volume is enormous and the serial lock becomes a bottleneck.
pub(crate) const VERBOSE_SCHED: bool = false;

/// `vsched!("...")` is `log!("...")` when VERBOSE_SCHED is on, otherwise a
/// no-op. The const-`if` lets the compiler dead-code-eliminate when off.
macro_rules! vsched {
    ($($arg:tt)*) => {
        if VERBOSE_SCHED { crate::log!($($arg)*); }
    };
}

/// Process-scoped thread identity. Tids are per-process, so the scheduler
/// uses TaskId to uniquely identify threads system-wide.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TaskId(pub Pid, pub Tid);

impl TaskId {
    /// Pack into a u64 for atomic storage (low 32 = tid, high 32 = pid).
    pub fn pack(self) -> u64 { self.1.raw() as u64 | (self.0.raw() as u64) << 32 }
    /// Unpack from a u64.
    pub fn unpack(v: u64) -> Self { Self(Pid::from_raw((v >> 32) as u32), Tid::from_raw(v as u32)) }
}

impl core::fmt::Display for TaskId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}:{}", self.0, self.1)
    }
}
// Per-CPU CPU-time counters. Each CPU only writes its own slot (no contention).
// Cache-line padded to prevent false sharing between cores.
#[repr(align(64))]
struct CpuTimeCounter(AtomicU64);
static CPU_TIME_NS: [CpuTimeCounter; MAX_CPUS] = [const { CpuTimeCounter(AtomicU64::new(0)) }; MAX_CPUS];

/// Returns cumulative CPU-nanoseconds consumed across all CPUs (monotonic).
pub fn total_cpu_ns() -> u64 {
    let mut total = 0u64;
    for i in 0..crate::arch::smp::cpu_count() as usize {
        total += CPU_TIME_NS[i].0.load(Ordering::Relaxed);
    }
    total
}

const EVENT_QUEUE_SIZE: usize = 256;

// ---------------------------------------------------------------------------
// In-schedule re-entry guard — per-CPU, lock-free
// ---------------------------------------------------------------------------
//
// Set by `do_schedule` on entry, cleared on every exit path. Read by
// `do_preempt` to suppress involuntary preempts that fire while we're still
// inside a `do_schedule` body (e.g. on the resume path of `run_task_on_self`,
// where releasing a lock runs `preempt::enable`, which calls `do_preempt`).
//
// Without this guard, a Ring 0 timer fire mid-resume sets `need_resched`,
// the next `preempt::enable` invokes `do_preempt` → `yield_now` →
// `do_schedule` recursively. Each recursion stacks another `do_schedule`
// frame (notably the ~2.7 KB `[EventSource; 256]` buffer in `drain_events`)
// on the kernel stack until the canary trips. Same bug class Linux solved
// with PREEMPT_ACTIVE.
//
// The guard transfers across `context_switch`: task A enters do_schedule on
// CPU N (sets), parks. Task B was previously parked on CPU N inside its own
// do_schedule frame; B resumes on CPU N, runs `handle_outgoing`, returns
// from its do_schedule (clears). Set/clear are paired one-per-frame on the
// CPU that's currently executing — read `cpu_id()` at clear, not at entry,
// because work-stealing can resume a task on a different CPU than it parked
// on, and the clear belongs to the resuming CPU.
static IN_SCHEDULE: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

/// Are we currently inside a `do_schedule` frame on this CPU?
pub fn in_schedule_self() -> bool {
    let cpu = percpu::cpu_id() as usize;
    IN_SCHEDULE[cpu].load(Ordering::Relaxed)
}

#[inline]
fn enter_schedule() {
    IN_SCHEDULE[percpu::cpu_id() as usize].store(true, Ordering::Relaxed);
}

#[inline]
fn leave_schedule() {
    IN_SCHEDULE[percpu::cpu_id() as usize].store(false, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// Poison set — lock-free, prevents panicked threads from being re-scheduled
// AND carries them to `cpu_idle_loop`, their only cleanup site
// ---------------------------------------------------------------------------

static POISONED: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(u64::MAX) }; MAX_CPUS];

/// Mark a thread as poisoned (panicked). Lock-free — the panic path can hold
/// any lock, so this is the only thing it is allowed to do.
///
/// The mark is not just "do not re-schedule": it is the request that gets the
/// thread zombified and its waiter woken, so losing one strands a `waitpid`
/// forever. One slot per CPU suffices — the panic path's next act is
/// `schedule_no_return` into this CPU's idle loop, which reaps every slot
/// before it picks another task, so a CPU cannot poison twice over one slot.
pub fn poison_tid(id: TaskId) {
    let cpu = percpu::cpu_id() as usize;
    // Bounded by AP bring-up, which indexes its own `MAX_CPUS` arrays by
    // cpu id. Neither arm below can fire; both scream rather than drop the
    // record, because dropping it is the wedge this set exists to prevent.
    let Some(slot) = POISONED.get(cpu) else {
        crate::log!("poison_tid: cpu {cpu} >= MAX_CPUS — {id} will never be reaped");
        return;
    };
    let prev = slot.swap(id.pack(), Ordering::Release);
    if prev != u64::MAX {
        crate::log!("poison_tid: cpu {cpu} slot still held {} — its waiter is stranded",
            TaskId::unpack(prev));
    }
}

fn is_poisoned(id: TaskId) -> bool {
    let needle = id.pack();
    POISONED.iter().any(|s| s.load(Ordering::Relaxed) == needle)
}

fn clear_poison(id: TaskId) {
    let needle = id.pack();
    for slot in &POISONED {
        let _ = slot.compare_exchange(needle, u64::MAX, Ordering::Relaxed, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Kill set + wake-transit counter — retire_task's visibility contract
// ---------------------------------------------------------------------------
//
// `retire_task` must prove a task is gone from the scheduler even though ctxs
// legally move between containers. Every move happens with the ctx observable
// under some lock (the idle loop's steal holds the destination CPU's queue
// for the whole hop; spawn holds the process table across insert+enqueue)
// EXCEPT two hops that carry the ctx on a stack: the wake path's pool→queue
// hop, and handle_outgoing's outgoing→{pool,queue,drop} hop. Two mechanisms
// close the gap:
//   - KILLED: once a TaskId is marked, every insertion point (ready-queue
//     insert, blocked-pool park, pick) drops the ctx instead of scheduling
//     it — the mark is checked under the destination's lock, so any hop
//     whose insert lands after the mark is terminal.
//   - CTX_TRANSITS: counts in-flight on-stack hops. A clean scan is only
//     trusted as "gone" while no hop is in flight, so a ctx popped *before*
//     the mark cannot complete its insert *after* retire_task unmarks and
//     returns (a completed-while-marked insert already dropped it).

const MAX_CONCURRENT_RETIRES: usize = 16;
static KILLED: [AtomicU64; MAX_CONCURRENT_RETIRES] =
    [const { AtomicU64::new(u64::MAX) }; MAX_CONCURRENT_RETIRES];

fn mark_killed(id: TaskId) {
    let packed = id.pack();
    for slot in &KILLED {
        if slot.compare_exchange(u64::MAX, packed, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
            return;
        }
    }
    panic!("mark_killed: more than {MAX_CONCURRENT_RETIRES} concurrent retires");
}

fn clear_killed(id: TaskId) {
    let packed = id.pack();
    for slot in &KILLED {
        if slot.compare_exchange(packed, u64::MAX, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
            return;
        }
    }
}

fn is_killed(id: TaskId) -> bool {
    let packed = id.pack();
    KILLED.iter().any(|s| s.load(Ordering::SeqCst) == packed)
}

static CTX_TRANSITS: AtomicU32 = AtomicU32::new(0);

/// RAII marker for an on-stack ctx hop. Wake paths hold it from before the
/// blocked-pool removal until after the ready-queue insert; handle_outgoing
/// holds it across its whole frame (take_outgoing until the ctx reaches the
/// pool, a queue, or is dropped).
struct TransitGuard;

impl TransitGuard {
    fn new() -> Self {
        CTX_TRANSITS.fetch_add(1, Ordering::SeqCst);
        TransitGuard
    }
}

impl Drop for TransitGuard {
    fn drop(&mut self) {
        CTX_TRANSITS.fetch_sub(1, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------
// EventSource — what wakes a blocked thread
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// PerCpuEventQueue — lock-free interrupt-to-scheduler channel
// ---------------------------------------------------------------------------

struct PerCpuEventQueue {
    events: [EventSource; EVENT_QUEUE_SIZE],
    head: AtomicU32, // writer (interrupt handler) — wait-free
    tail: AtomicU32, // reader (scheduler) — single consumer
    overflow_count: AtomicU64, // events dropped due to full buffer
}

impl PerCpuEventQueue {
    const fn new() -> Self {
        Self {
            events: [EventSource::Keyboard; EVENT_QUEUE_SIZE],
            head: AtomicU32::new(0),
            tail: AtomicU32::new(0),
            overflow_count: AtomicU64::new(0),
        }
    }

    /// Push an event from interrupt context. Wait-free, no locks.
    fn push(&self, event: EventSource) {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        let next = (head + 1) % EVENT_QUEUE_SIZE as u32;
        if next == tail {
            self.overflow_count.fetch_add(1, Ordering::Relaxed);
            return;
        }
        // SAFETY: single producer per CPU, index is in bounds
        unsafe {
            let slot = &self.events as *const _ as *mut EventSource;
            slot.add(head as usize).write(event);
        }
        self.head.store(next, Ordering::Release);
    }

    /// Non-consuming emptiness check, for the idle loop's pre-hlt re-check.
    fn has_events(&self) -> bool {
        self.head.load(Ordering::Acquire) != self.tail.load(Ordering::Acquire)
    }

    /// Drain all pending events. Called from scheduler context.
    fn drain_into(&self, buf: &mut [EventSource; EVENT_QUEUE_SIZE], count: &mut usize) {
        *count = 0;
        loop {
            let tail = self.tail.load(Ordering::Relaxed);
            let head = self.head.load(Ordering::Acquire);
            if tail == head {
                break;
            }
            if *count >= EVENT_QUEUE_SIZE {
                break;
            }
            // SAFETY: single consumer, index is in bounds
            buf[*count] = unsafe {
                let slot = &self.events as *const EventSource;
                slot.add(tail as usize).read()
            };
            *count += 1;
            self.tail.store((tail + 1) % EVENT_QUEUE_SIZE as u32, Ordering::Release);
        }
    }
}

// SAFETY: PerCpuEventQueue uses atomics for synchronization.
unsafe impl Sync for PerCpuEventQueue {}

static PERCPU_EVENTS: [PerCpuEventQueue; MAX_CPUS] =
    [const { PerCpuEventQueue::new() }; MAX_CPUS];

/// Push an event from interrupt context. Wait-free, no locks, safe from any context.
pub fn push_event(event: EventSource) {
    let cpu = percpu::cpu_id() as usize;
    PERCPU_EVENTS[cpu].push(event);
}

// ---------------------------------------------------------------------------
// TaskCtx — context switch state, owned by the scheduler
// ---------------------------------------------------------------------------

pub struct TaskCtx {
    pub id: TaskId,
    pub kernel_stack: OwnedAlloc,
    pub kernel_rsp: u64,
    pub address_space: Option<PageTables>,
    pub fs_base: u64,
    pub cpu_ns: u64,
    pub scheduled_at: u64,
    pub blocked_on: Option<EventSource>, // what this task is waiting on (None = pure timeout/wake_task)
    pub deadline: u64, // 0 = no deadline
    pub blocked_since: u64, // nanos_since_boot when entered blocked pool (0 = not blocked)
    pub enqueued_at: u64, // nanos_since_boot when inserted into ready queue (0 = not queued)
    pub is_rt: bool, // RT band (FIFO scheduling, preempts normal)
    pub rt_inherited: bool, // RT was transiently inherited via priority inheritance
    pub accounting: TaskAccounting,
    pub last_cpu: Option<u32>, // CPU this task last ran on; None = never run
}

/// Per-thread accounting counters, flushed to ProcessAccounting on exit.
#[derive(Default)]
pub struct TaskAccounting {
    pub blocked_io_ns: u64,
    pub blocked_futex_ns: u64,
    pub blocked_pipe_ns: u64,
    pub blocked_ipc_ns: u64,
    pub blocked_other_ns: u64,
    pub runqueue_wait_ns: u64,
}

impl TaskAccounting {
    /// Merge this thread's counters into the process-level accounting.
    pub fn merge_into(&self, proc_acct: &mut process::ProcessAccounting) {
        proc_acct.blocked_io_ns += self.blocked_io_ns;
        proc_acct.blocked_futex_ns += self.blocked_futex_ns;
        proc_acct.blocked_pipe_ns += self.blocked_pipe_ns;
        proc_acct.blocked_ipc_ns += self.blocked_ipc_ns;
        proc_acct.blocked_other_ns += self.blocked_other_ns;
        proc_acct.runqueue_wait_ns += self.runqueue_wait_ns;
    }
}

impl TaskCtx {
    pub fn kernel_stack_top(&self) -> u64 {
        self.kernel_stack.ptr() as u64 + KERNEL_STACK_SIZE as u64
    }

    pub fn cr3(&self) -> crate::mm::paging::Cr3 {
        self.address_space.as_ref().unwrap().lock().cr3()
    }

    pub fn stop_cpu_timer(&mut self, now: u64) {
        if self.scheduled_at > 0 {
            let delta = now - self.scheduled_at;
            self.cpu_ns += delta;
            self.scheduled_at = 0;
            let cpu = percpu::cpu_id() as usize;
            CPU_TIME_NS[cpu].0.fetch_add(delta, Ordering::Relaxed);
        }
    }

    pub fn start_cpu_timer(&mut self, now: u64) {
        self.scheduled_at = now;
    }

    pub fn cpu_ns(&self) -> u64 {
        if self.scheduled_at > 0 {
            self.cpu_ns + (crate::hw::now_ns() - self.scheduled_at)
        } else {
            self.cpu_ns
        }
    }

    /// Accumulate blocked time from blocked_since/blocked_on into per-category counters.
    /// Called when a thread is removed from the blocked pool before re-enqueuing.
    fn accumulate_blocked_time(&mut self) {
        if self.blocked_since == 0 { return; }
        let elapsed = crate::hw::now_ns() - self.blocked_since;
        let acct = &mut self.accounting;
        match &self.blocked_on {
            Some(EventSource::IoUring(_)) => acct.blocked_io_ns += elapsed,
            Some(EventSource::Futex(_)) => acct.blocked_futex_ns += elapsed,
            Some(EventSource::PipeReadable(_) | EventSource::PipeWritable(_)) => acct.blocked_pipe_ns += elapsed,
            Some(EventSource::Listener(_)) => acct.blocked_ipc_ns += elapsed,
            _ => acct.blocked_other_ns += elapsed,
        }
        self.blocked_since = 0;
        self.blocked_on = None;
    }
}

// ---------------------------------------------------------------------------
// WokenBatch — compiler-enforced thread leak prevention
// ---------------------------------------------------------------------------

#[must_use = "woken threads must be enqueued or they are permanently lost"]
pub struct WokenBatch {
    threads: Vec<TaskCtx>,
}

impl WokenBatch {
    fn new() -> Self {
        Self { threads: Vec::new() }
    }

    fn push(&mut self, ctx: TaskCtx) {
        self.threads.push(ctx);
    }

    fn is_empty(&self) -> bool {
        self.threads.is_empty()
    }
}

// ---------------------------------------------------------------------------
// SwitchReason — disposition of the outgoing thread (no heap allocation)
// ---------------------------------------------------------------------------

enum SwitchReason {
    Yield,
    Block {
        event: Option<EventSource>,
        deadline: u64,
    },
    Exit,
}

// ---------------------------------------------------------------------------
// CpuRunQueue + CpuQueueGuard — per-CPU ready queue with typed lock ordering
// ---------------------------------------------------------------------------

struct CpuRunQueue {
    current: Option<TaskCtx>,
    outgoing: Option<(TaskCtx, SwitchReason)>,
    save_rsp: u64,
    rt_ready: VecDeque<TaskCtx>,
    /// Normal band, ordered by (vruntime, insertion sequence). The second
    /// key component is deliberately NOT TaskId: all threads of a process
    /// share one per-Pid vruntime, so an id tie-break is deterministic —
    /// the lowest tid wins every tie, and its same-process siblings only
    /// run when it blocks (observed: Doom's midi thread starving behind
    /// the game thread on a single core). A monotonic per-queue insertion
    /// sequence makes equal-vruntime entries FIFO instead: a re-inserted
    /// thread goes behind its same-vruntime siblings, so threads of one
    /// process round-robin — without gaining any cross-process share,
    /// because the shared per-Pid vruntime still governs the primary key.
    ready: BTreeMap<(u64, u64), TaskCtx>,
    insert_seq: u64,
}

impl CpuRunQueue {
    const fn new() -> Self {
        Self {
            current: None,
            outgoing: None,
            save_rsp: 0,
            rt_ready: VecDeque::new(),
            ready: BTreeMap::new(),
            insert_seq: 0,
        }
    }
}

/// Typed guard for a locked CpuRunQueue. Lock ordering enforced by API:
/// `charge()` acquires sched_state internally, guaranteeing CPU queue →
/// sched_state. Compiler prevents wrong ordering.
///
/// `.1` is the cpus[N] index this guard holds — used by VERBOSE_SCHED logs
/// to identify which queue is being released. Costs 4 bytes per guard.
pub struct CpuQueueGuard<'a>(crate::sync::LockGuard<'a, CpuRunQueue>, u32);

impl Drop for CpuQueueGuard<'_> {
    fn drop(&mut self) {
        vsched!("sched drop cpus[{}]", self.1);
        // self.0 (LockGuard) drops next, performing the actual unlock.
    }
}

impl<'a> CpuQueueGuard<'a> {
    pub fn pick_next(&mut self) -> Option<(u64, TaskCtx)> {
        // RT band first (FIFO)
        while let Some(mut ctx) = self.0.rt_ready.pop_front() {
            if is_poisoned(ctx.id) || is_killed(ctx.id) {
                // Dropped, not run: the thread leaves the runnable set here,
                // so the per-process refcount must follow.
                SCHEDULER.leave_runnable(ctx.id.0);
                continue;
            }
            if ctx.enqueued_at > 0 {
                ctx.accounting.runqueue_wait_ns += crate::hw::now_ns() - ctx.enqueued_at;
                ctx.enqueued_at = 0;
            }
            return Some((0, ctx));
        }
        // Normal band (CFS — lowest vruntime first)
        while let Some(((vrt, _), mut ctx)) = self.0.ready.pop_first() {
            if is_poisoned(ctx.id) || is_killed(ctx.id) {
                SCHEDULER.leave_runnable(ctx.id.0);
                continue;
            }
            if ctx.enqueued_at > 0 {
                ctx.accounting.runqueue_wait_ns += crate::hw::now_ns() - ctx.enqueued_at;
                ctx.enqueued_at = 0;
            }
            return Some((vrt, ctx));
        }
        None
    }

    pub fn insert(&mut self, vrt: u64, mut ctx: TaskCtx) {
        if is_killed(ctx.id) {
            // Retirement in progress: dropping here instead of queueing is
            // the terminal hand-off that lets retire_task trust a clean scan.
            SCHEDULER.leave_runnable(ctx.id.0);
            return;
        }
        ctx.enqueued_at = crate::hw::now_ns();
        if ctx.is_rt {
            self.0.rt_ready.push_back(ctx);
        } else {
            // Unique, monotonic per queue — see the `ready` field doc for
            // why the tie-break is insertion order rather than TaskId.
            self.0.insert_seq += 1;
            let seq = self.0.insert_seq;
            self.0.ready.insert((vrt, seq), ctx);
        }
    }

    pub fn take_current(&mut self) -> Option<TaskCtx> { self.0.current.take() }

    pub fn set_current(&mut self, ctx: TaskCtx) {
        // An occupied slot here means a previous task's ctx was abandoned
        // (e.g. a panic-recovery rejoin that skipped the outgoing path) —
        // assigning over it would silently drop a live thread's kernel stack
        // and address-space reference.
        assert!(self.0.current.is_none(),
            "set_current: cpus[{}] already has a current task", self.1);
        self.0.current = Some(ctx);
    }
    pub fn current(&self) -> Option<&TaskCtx> { self.0.current.as_ref() }
    pub fn current_mut(&mut self) -> Option<&mut TaskCtx> { self.0.current.as_mut() }
    fn take_outgoing(&mut self) -> Option<(TaskCtx, SwitchReason)> { self.0.outgoing.take() }
    fn outgoing_id(&self) -> Option<TaskId> { self.0.outgoing.as_ref().map(|(c, _)| c.id) }

    fn set_outgoing(&mut self, ctx: TaskCtx, reason: SwitchReason) {
        assert!(self.0.outgoing.is_none(),
            "set_outgoing: cpus[{}] already has an unhandled outgoing task", self.1);
        self.0.outgoing = Some((ctx, reason));
    }
    pub fn save_rsp_ptr(&mut self) -> *mut u64 { &mut self.0.save_rsp as *mut u64 }
    pub fn save_rsp(&self) -> u64 { self.0.save_rsp }
    pub fn ready_len(&self) -> usize { self.0.rt_ready.len() + self.0.ready.len() }

    pub fn is_ready(&self, id: TaskId) -> bool {
        self.0.rt_ready.iter().any(|c| c.id == id) ||
        self.0.ready.values().any(|c| c.id == id)
    }

    pub fn remove_ready(&mut self, id: TaskId) -> Option<TaskCtx> {
        if let Some(pos) = self.0.rt_ready.iter().position(|c| c.id == id) {
            return self.0.rt_ready.remove(pos);
        }
        let key = self.0.ready.iter().find(|(_, c)| c.id == id).map(|(&k, _)| k);
        key.and_then(|k| self.0.ready.remove(&k))
    }

    pub fn charge(&self, sched: &Scheduler, process: Pid, ns: u64) {
        sched.charge_vruntime(process, ns);
    }

    /// Find `id` anywhere in this queue — current, outgoing, either ready
    /// band — and report its cumulative CPU time. A running thread includes
    /// the live slice since `scheduled_at` (see `TaskCtx::cpu_ns`).
    fn find_cpu_ns(&self, id: TaskId) -> Option<u64> {
        if let Some(ctx) = self.0.current.as_ref() {
            if ctx.id == id { return Some(ctx.cpu_ns()); }
        }
        if let Some((ctx, _)) = self.0.outgoing.as_ref() {
            if ctx.id == id { return Some(ctx.cpu_ns()); }
        }
        self.0.rt_ready.iter()
            .chain(self.0.ready.values())
            .find(|c| c.id == id)
            .map(|c| c.cpu_ns())
    }

    pub fn into_raw(self) {
        vsched!("sched into_raw cpus[{}]", self.1);
        // Forget the whole wrapper so neither our Drop nor the inner
        // LockGuard's Drop runs — lock stays held across context_switch.
        core::mem::forget(self);
    }
}

// ---------------------------------------------------------------------------
// BlockedPool — event-indexed blocked threads with deadline heap
// ---------------------------------------------------------------------------

/// A thread that has registered on a wait queue but has not reached the pool
/// yet — spec §8.1's `Committing` state, in the shape the old pool can hold.
///
/// The registration is what makes the park window harmless: a wake that finds
/// no parked waiter marks `fired` here instead, and `park_outgoing` honors the
/// mark rather than parking. Both sides run under the pool lock, so a wake
/// either marks the registration or finds the thread already parked.
struct Prepared {
    source: Option<EventSource>,
    fired: bool,
}

struct BlockedPool {
    threads: HashMap<TaskId, TaskCtx>,
    by_event: BTreeMap<EventSource, Vec<TaskId>>,
    deadlines: BTreeMap<(u64, TaskId), TaskId>,
    /// Registrations of threads still in their park window (see `Prepared`).
    prepared: HashMap<TaskId, Prepared>,
    /// Sticky wakes: `wake_task` targets that were not in the pool and hold no
    /// registration either — a park window belonging to a source that stage 5
    /// has not converted yet. Consumed at park time so a waitpid/thread_join
    /// wake can never vanish into the window. A leftover entry for a task that
    /// never parks again costs one spurious wake after Tid reuse — harmless,
    /// all block paths retry in a loop.
    pending_wakes: HashSet<TaskId>,
}

impl BlockedPool {
    fn new() -> Self {
        Self {
            threads: HashMap::new(),
            by_event: BTreeMap::new(),
            deadlines: BTreeMap::new(),
            prepared: HashMap::new(),
            pending_wakes: HashSet::new(),
        }
    }

    /// Mark up to `limit` registrations for `event`. A marked registration
    /// counts as a woken waiter: its park will decline and the thread runs on.
    fn fire_prepared(&mut self, event: &EventSource, limit: usize) -> usize {
        let mut fired = 0;
        for p in self.prepared.values_mut() {
            if fired == limit {
                break;
            }
            if p.source == Some(*event) && !p.fired {
                p.fired = true;
                fired += 1;
            }
        }
        fired
    }

    fn insert(&mut self, mut ctx: TaskCtx) {
        let id = ctx.id;
        ctx.blocked_since = crate::hw::now_ns();
        if let Some(event) = ctx.blocked_on {
            self.by_event.entry(event)
                .or_insert_with(Vec::new)
                .push(id);
        }
        if ctx.deadline > 0 {
            self.deadlines.insert((ctx.deadline, id), id);
        }
        self.threads.insert(id, ctx);
    }

    /// Remove a thread from all indexes. Single cleanup path.
    fn remove_task(&mut self, id: TaskId) -> Option<TaskCtx> {
        let ctx = self.threads.remove(&id)?;
        let tag = ctx.blocked_on.as_ref()
            .map(crate::trace::event_source_tag)
            .unwrap_or(0xFF_000000); // 0xFF = no event (deadline/explicit wake)
        crate::trace::trace(crate::trace::Kind::Wake, tag);
        if let Some(event) = &ctx.blocked_on {
            if let Some(waiters) = self.by_event.get_mut(event) {
                waiters.retain(|&k| k != id);
                if waiters.is_empty() {
                    self.by_event.remove(event);
                }
            }
        }
        if ctx.deadline > 0 {
            self.deadlines.remove(&(ctx.deadline, id));
        }
        Some(ctx)
    }

    /// Wake all threads waiting on an event source into a batch, and mark
    /// every registration for it — the waiters still in their park window.
    fn take_by_event_into(&mut self, event: &EventSource, batch: &mut WokenBatch) {
        self.fire_prepared(event, usize::MAX);
        let Some(waiters) = self.by_event.remove(event) else { return };
        for id in waiters {
            if let Some(ctx) = self.remove_task(id) {
                batch.push(ctx);
            }
        }
    }

    /// Wake up to `count` threads waiting on an event source: parked ones
    /// first, then registrations in the park window. Returns how many were
    /// woken in total.
    fn take_by_event_limited(&mut self, event: &EventSource, count: usize, batch: &mut WokenBatch) -> usize {
        let mut woken = 0;
        if let Some(waiters) = self.by_event.get_mut(event) {
            let n = count.min(waiters.len());
            let ids_to_wake: Vec<TaskId> = waiters.drain(..n).collect();
            if waiters.is_empty() {
                self.by_event.remove(event);
            }
            for id in ids_to_wake {
                if let Some(ctx) = self.remove_task(id) {
                    batch.push(ctx);
                    woken += 1;
                }
            }
        }
        woken + self.fire_prepared(event, count - woken)
    }
}

// ---------------------------------------------------------------------------
// Scheduler — the global scheduler instance
// ---------------------------------------------------------------------------

pub struct Scheduler {
    cpus: [Lock<CpuRunQueue>; MAX_CPUS],
    blocked: Lock<Option<BlockedPool>>,
    /// Per-process fair-share state. The runnable/non-runnable state machine
    /// and all vruntime/lag math live in `toyos_sched::fair` (scheduler-core
    /// migration Stage 1); this map and its lock stay kernel-owned until the
    /// cutover gives each task its own share.
    sched_state: Lock<Option<HashMap<Pid, ShareState>>>,
    min_vruntime: Frontier,
}

static SCHEDULER: Scheduler = Scheduler {
    cpus: [const { Lock::new(CpuRunQueue::new()) }; MAX_CPUS],
    blocked: Lock::new(None),
    sched_state: Lock::new(None),
    min_vruntime: Frontier::new(),
};

pub fn init() {
    *SCHEDULER.blocked.lock() = Some(BlockedPool::new());
    *SCHEDULER.sched_state.lock() = Some(HashMap::new());
}

/// Log scheduler health. Called from idle loop.
pub fn log_health() {
    let mut ready = 0usize;
    for i in 0..crate::arch::smp::cpu_count() as usize {
        if let Some(q) = SCHEDULER.try_lock_cpu(i) {
            ready += q.ready_len();
            if q.current().is_some() { ready += 1; }
        }
    }
    let blocked = SCHEDULER.blocked.try_lock()
        .map(|g| g.as_ref().map(|p| p.threads.len()).unwrap_or(0))
        .unwrap_or(0);
    let tid = percpu::current_tid();
    crate::log!("sched: ready={} blocked={} current={:?}", ready, blocked, tid);

    // Dump blocked threads at most every 10s (serial output is slow and can
    // starve audio DMA completions on single-core systems)
    use core::sync::atomic::AtomicU64;
    static NEXT_BLOCKED_DUMP: AtomicU64 = AtomicU64::new(0);
    const BLOCKED_DUMP_INTERVAL_NS: u64 = 10_000_000_000;
    let now_bl = crate::hw::now_ns();
    if ready == 0 && blocked > 0 && now_bl >= NEXT_BLOCKED_DUMP.load(Ordering::Relaxed) {
        NEXT_BLOCKED_DUMP.store(now_bl + BLOCKED_DUMP_INTERVAL_NS, Ordering::Relaxed);
        dump_blocked();
    }

    // PMM stats dump (any CPU, time-gated to every 10s)
    static NEXT_PMM_DUMP: AtomicU64 = AtomicU64::new(0);
    const PMM_DUMP_INTERVAL_NS: u64 = 10_000_000_000;
    let now = crate::hw::now_ns();
    let next = NEXT_PMM_DUMP.load(Ordering::Relaxed);
    if next == 0 {
        NEXT_PMM_DUMP.store(now + PMM_DUMP_INTERVAL_NS, Ordering::Relaxed);
    } else if now >= next {
        // CAS to avoid multiple CPUs dumping simultaneously
        if NEXT_PMM_DUMP.compare_exchange(next, now + PMM_DUMP_INTERVAL_NS,
            Ordering::Relaxed, Ordering::Relaxed).is_ok()
        {
            crate::mm::pmm::dump_stats();
        }
    }
}

impl Scheduler {
    fn lock_cpu(&self, cpu: usize) -> CpuQueueGuard<'_> {
        vsched!("sched lock_cpu({}) acquiring", cpu);
        let guard = CpuQueueGuard(self.cpus[cpu].lock(), cpu as u32);
        vsched!("sched lock_cpu({}) acquired", cpu);
        guard
    }

    fn try_lock_cpu(&self, cpu: usize) -> Option<CpuQueueGuard<'_>> {
        let r = self.cpus[cpu].try_lock().map(|g| CpuQueueGuard(g, cpu as u32));
        vsched!("sched try_lock_cpu({}) -> {}", cpu, if r.is_some() { "ok" } else { "fail" });
        r
    }

    /// Transition: a thread of `pid` is becoming runnable. Returns the
    /// vruntime to insert with. New pids start at the current frontier with
    /// zero lag; the transition math is `ShareState::enter_runnable`.
    fn enter_runnable(&self, pid: Pid) -> u64 {
        let min = self.min_vruntime.get();
        let mut state = self.sched_state.lock_unwrap();
        match state.entry(pid) {
            hashbrown::hash_map::Entry::Vacant(v) => {
                v.insert(ShareState::new_runnable(min));
                min
            }
            hashbrown::hash_map::Entry::Occupied(mut o) => o.get_mut().enter_runnable(min),
        }
    }

    /// Transition: a thread of `pid` is no longer runnable (blocked,
    /// exited, killed).
    ///
    /// No-op if the process is already NonRunnable or absent — covers the
    /// case where `remove_task` is called on a thread already in the
    /// blocked pool (refcount was already decremented when it blocked).
    fn leave_runnable(&self, pid: Pid) {
        let min = self.min_vruntime.get();
        let mut state = self.sched_state.lock_unwrap();
        let Some(s) = state.get_mut(&pid) else { return };
        s.leave_runnable(min);
    }

    fn charge_vruntime(&self, process: Pid, ns: u64) {
        let mut state = self.sched_state.lock_unwrap();
        let Some(s) = state.get_mut(&process) else { return };
        if s.charge(ns).is_err() {
            panic!("charge_vruntime: pid {process} is NonRunnable");
        }
    }

    /// Read the current Runnable vruntime. Called from the Yield re-insert
    /// path where the thread stays runnable. Panics if NonRunnable, which
    /// would indicate a bookkeeping bug (a yielding thread was wrongly
    /// counted out).
    ///
    /// Untracked is legal in exactly one situation: a thread of an
    /// already-reaped process finishing its exit path (the parent's waitpid
    /// dropped the ProcessEntry — and with it the sched_state entry — while
    /// the exiting thread had one more yield before exit_current). Re-enter
    /// at the global floor for that final trip.
    fn current_runnable_vruntime(&self, process: Pid) -> u64 {
        let min = self.min_vruntime.get();
        let state = self.sched_state.lock_unwrap();
        match state.get(&process) {
            Some(s) => s.runnable_vruntime().unwrap_or_else(|_| {
                panic!("current_runnable_vruntime: pid {process} is NonRunnable")
            }),
            None => min,
        }
    }

    fn remove_vruntime(&self, process: Pid) {
        self.sched_state.lock_unwrap().remove(&process);
    }

    fn pick_target_cpu(&self) -> u32 {
        let count = crate::arch::smp::cpu_count();
        let self_cpu = percpu::cpu_id();
        // Tie → self for cache locality. Initialising to 0 unconditionally
        // funnels every wake to cpu0 whenever loads are equal.
        let mut best_cpu = self_cpu;
        let mut best_load = usize::MAX;
        for i in 0..count {
            if let Some(q) = self.try_lock_cpu(i as usize) {
                // Load = queued + currently-running. Without `current`, an
                // idle sibling (current=None, ready=0) and a busy local CPU
                // (current=Some, ready=0) both report 0 and the first index
                // wins — every wake lands on cpu0 even when cpu1 is idle.
                let load = q.ready_len() + q.current().is_some() as usize;
                if load < best_load {
                    best_load = load;
                    best_cpu = i;
                }
            }
        }
        best_cpu
    }

    fn enqueue_batch(&self, batch: WokenBatch) {
        let self_cpu = percpu::cpu_id();
        let mut kick_mask: u64 = 0;
        for mut ctx in batch.threads {
            ctx.accumulate_blocked_time();
            let id = ctx.id;
            let vrt = self.enter_runnable(ctx.id.0);
            let cpu = self.pick_target_cpu();
            let is_rt = ctx.is_rt;
            vsched!("sched enqueue_batch {} -> cpus[{}] vrt={}", id, cpu, vrt);
            let mut q = self.lock_cpu(cpu as usize);
            q.insert(vrt, ctx);
            drop(q);
            if cpu != self_cpu {
                kick_mask |= 1 << cpu;
            } else if is_rt {
                // §9.4: an RT wake on this CPU must preempt the current
                // normal task at the next preempt point instead of waiting
                // out its quantum.
                HW.need_resched(CpuId(cpu));
            }
        }
        while kick_mask != 0 {
            let cpu = kick_mask.trailing_zeros();
            kick_mask &= kick_mask - 1;
            HW.kick(CpuId(cpu));
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub fn remove_vruntime(process: Pid) {
    SCHEDULER.remove_vruntime(process);
}

/// Live vruntime for `process` (see `ShareState::vruntime`). Untracked
/// processes report 0.
pub fn process_vruntime(process: Pid) -> u64 {
    let min = SCHEDULER.min_vruntime.get();
    let state = SCHEDULER.sched_state.lock_unwrap();
    state.get(&process).map_or(0, |s| s.vruntime(min))
}

/// Contract lag for `process` (see `ShareState::lag`): bounded
/// ±MAX_VRUNTIME_LAG_NS by construction, NOT the live `min - vruntime`
/// drift. If you need the live drift, compute `global_min_vruntime() -
/// process_vruntime(pid)` at the call site — but be aware it can exceed the
/// bound during the wake-to-pick gap on multi-CPU systems. Untracked
/// processes report 0.
pub fn process_lag(process: Pid) -> i64 {
    let state = SCHEDULER.sched_state.lock_unwrap();
    state.get(&process).map_or(0, |s| s.lag())
}

pub fn global_min_vruntime() -> u64 {
    SCHEDULER.min_vruntime.get()
}

pub fn enqueue_new(ctx: TaskCtx) {
    let id = ctx.id;
    let vrt = SCHEDULER.enter_runnable(ctx.id.0);
    let cpu = SCHEDULER.pick_target_cpu();
    let is_rt = ctx.is_rt;
    vsched!("sched enqueue_new {} -> cpus[{}] vrt={}", id, cpu, vrt);
    let mut q = SCHEDULER.lock_cpu(cpu as usize);
    q.insert(vrt, ctx);
    drop(q);
    if cpu != percpu::cpu_id() {
        HW.kick(CpuId(cpu));
    } else if is_rt {
        HW.need_resched(CpuId(cpu));
    }
}

/// Block the current thread on an optional event source with optional deadline.
/// `deadline = 0` means no timeout. `event = None` means woken only by `wake_task` or deadline.
///
/// The unconverted sources of spec stage 5 still enter here; converted ones
/// come through [`block_on`], which cannot be called without a registration.
pub fn block(event: Option<EventSource>, deadline: u64) {
    do_schedule(SwitchReason::Block { event, deadline });
}

/// Park the running thread on the queue it registered with.
///
/// Taking the ticket by value is the whole point: a park that reaches the pool
/// without a registration behind it is the lost-wake window, and there is no
/// other way to construct one.
pub fn block_on(ticket: WaitTicket<'_>, deadline: u64) {
    do_schedule(SwitchReason::Block { event: ticket.into_park(), deadline });
}

/// Register the running thread on `source` — `WaitQueue::prepare_wait`'s half
/// inside the pool. Returns the registered task, which the ticket carries.
pub(crate) fn register_wait(source: Option<EventSource>) -> TaskId {
    let id = TaskId(
        percpu::current_pid().expect("prepare_wait: no current process"),
        percpu::current_tid().expect("prepare_wait: no current thread"),
    );
    let prev = SCHEDULER.blocked.lock_unwrap().prepared.insert(
        id,
        Prepared { source, fired: false },
    );
    assert!(prev.is_none(), "prepare_wait: {id} is already registered on a queue");
    id
}

/// Withdraw a registration — `WaitTicket::cancel`.
pub(crate) fn cancel_wait(id: TaskId) {
    let prev = SCHEDULER.blocked.lock_unwrap().prepared.remove(&id);
    assert!(prev.is_some(), "cancel_wait: {id} holds no registration");
}

pub fn yield_now() {
    do_schedule(SwitchReason::Yield);
}

/// Unified preempt entry — timer Ring 3 path, `kernel_exit_to_user_check`,
/// and the `preempt::enable` slow path all funnel through here.
///
/// Owns the `need_resched` clear: a request is cleared exactly once, by the
/// code that acts on it. The one non-clearing return is the re-entry guard —
/// if this CPU is already inside a `do_schedule` frame (e.g. we got here
/// from `preempt::enable` on a lock release on the resume path of
/// `run_task_on_self`), the request stays set so the next non-nested
/// preempt poll acts on it. Same shape as Linux's PREEMPT_ACTIVE.
pub fn do_preempt() {
    if in_schedule_self() {
        return;
    }
    crate::preempt::clear_need_resched();
    if percpu::current_tid().is_none() {
        // Idle context — the idle loop is already scheduling; the request
        // is moot, not deferred.
        return;
    }
    crate::trace::trace(crate::trace::Kind::Preempt, 0);
    yield_now();
}

/// Check and wake threads with expired deadlines.
/// Called from drain_events (which already holds the blocked pool lock).
fn check_deadlines_locked(pool: &mut BlockedPool, batch: &mut WokenBatch) {
    let now = crate::hw::now_ns();
    while let Some((&(deadline, id), _)) = pool.deadlines.first_key_value() {
        if deadline > now { break; }
        pool.deadlines.pop_first();
        if let Some(ctx) = pool.remove_task(id) {
            batch.push(ctx);
        }
    }
}


pub fn exit_current(code: i32) -> ! {
    {
        let mut guard = process::PROCESS_TABLE.lock();
        let table = guard.as_mut().unwrap();
        let tid = percpu::current_tid().unwrap();
        let pid = percpu::current_pid().unwrap();
        process::zombify_tid(table,pid, tid, code);
    }
    do_schedule(SwitchReason::Exit);
    unreachable!("exit_current: returned from schedule");
}

pub fn schedule_no_return() -> ! {
    // Clear the CPU identity first: any preempt poll fired by a lock release
    // below must see "idle" and return instead of re-entering do_schedule
    // from this abandoned context.
    percpu::set_current_tid(None);
    percpu::set_current_pid(None);

    // Panic recovery arrives here with the faulted thread's ctx still parked
    // in `current`. It must leave through the normal outgoing→drop path
    // (handle_outgoing on the next switch-in frees it exactly once) — the
    // next set_current asserts an empty slot, and dropping it right here
    // would free the very kernel stack we are still running on.
    let cpu = percpu::cpu_id() as usize;
    match SCHEDULER.try_lock_cpu(cpu) {
        Some(mut q) => {
            if let Some(stale) = q.take_current() {
                if q.outgoing_id().is_some() {
                    // A parked thread awaits handle_outgoing; overwriting it
                    // would silently kill it. The scheduler on this CPU is
                    // mid-transition — rejoining cannot be made safe.
                    crate::log!("schedule_no_return: cpus[{cpu}] outgoing occupied, cannot rejoin");
                    crate::arch::apic::halt_all_cpus();
                }
                q.set_outgoing(stale, SwitchReason::Exit);
            }
        }
        None => {
            // Most likely this CPU's own interrupted scheduler path holds the
            // lock — rejoining would corrupt the queue. Die loudly.
            crate::log!("schedule_no_return: cpus[{cpu}] locked, cannot rejoin");
            crate::arch::apic::halt_all_cpus();
        }
    }

    // The abandoned context may have died inside a do_schedule frame; its
    // IN_SCHEDULE flag would otherwise suppress preemption on this CPU
    // forever (and trip the exit-to-user assert).
    leave_schedule();

    unsafe { percpu::set_kernel_stack(percpu::idle_stack_top()); }
    unsafe { crate::mm::paging::kernel_cr3().activate(); }
    let sp = percpu::idle_stack_top();
    unsafe {
        asm!(
            "mov rsp, {sp}",
            "jmp {func}",
            sp = in(reg) sp,
            func = in(reg) cpu_idle_loop as *const () as usize,
            options(noreturn),
        );
    }
}

/// Wake all threads waiting on a specific event source.
pub fn wake_by_event(event: EventSource) {
    let _transit = TransitGuard::new();
    let batch = {
        let mut pool = SCHEDULER.blocked.lock_unwrap();
        let mut batch = WokenBatch::new();
        pool.take_by_event_into(&event, &mut batch);
        batch
    };
    if !batch.is_empty() {
        SCHEDULER.enqueue_batch(batch);
    }
}

/// Wake pipe readers with priority inheritance: if the caller is RT,
/// boost woken threads to RT (transient — cleared when they next block).
/// The pipe is also marked so a reader that was runnable (not blocked) at
/// write time still inherits the boost when it consumes the data.
pub fn wake_pipe_readers(pipe_id: PipeId) {
    let caller_is_rt = {
        let cpu = percpu::cpu_id() as usize;
        SCHEDULER.lock_cpu(cpu).current().map_or(false, |c| c.is_rt)
    };
    if caller_is_rt {
        crate::pipe::set_rt_boost_pending(pipe_id);
    }

    let _transit = TransitGuard::new();
    let batch = {
        let mut pool = SCHEDULER.blocked.lock_unwrap();
        let mut batch = WokenBatch::new();
        pool.take_by_event_into(&EventSource::PipeReadable(pipe_id), &mut batch);
        if caller_is_rt {
            for ctx in batch.threads.iter_mut() {
                if !ctx.is_rt {
                    ctx.is_rt = true;
                    ctx.rt_inherited = true;
                }
            }
        }
        batch
    };
    if !batch.is_empty() {
        SCHEDULER.enqueue_batch(batch);
    }
}

/// Wake pipe writers: threads blocked on PipeWritable(pipe_id) + poll threads interested in this pipe.
pub fn wake_pipe_writers(pipe_id: PipeId) {
    wake_by_event(EventSource::PipeWritable(pipe_id));
}

/// Wake a specific thread (for waitpid/thread_join).
pub fn wake_task(id: TaskId) {
    if is_poisoned(id) { return; }
    let _transit = TransitGuard::new();
    let mut ctx = {
        let mut pool = SCHEDULER.blocked.lock_unwrap();
        match pool.remove_task(id) {
            Some(ctx) => ctx,
            None => {
                // Not in the pool: either running/ready (wake is moot) or in
                // its park window — committed to blocking but not yet
                // inserted. A registered thread takes the wake on its ticket,
                // whatever source it registered on; the rest need the sticky
                // set until stage 5 has converted them.
                match pool.prepared.get_mut(&id) {
                    Some(p) => p.fired = true,
                    None => { pool.pending_wakes.insert(id); }
                }
                return;
            }
        }
    };
    ctx.accumulate_blocked_time();
    let vrt = SCHEDULER.enter_runnable(ctx.id.0);
    let cpu = SCHEDULER.pick_target_cpu();
    let is_rt = ctx.is_rt;
    let mut q = SCHEDULER.lock_cpu(cpu as usize);
    q.insert(vrt, ctx);
    drop(q);
    if cpu != percpu::cpu_id() {
        HW.kick(CpuId(cpu));
    } else if is_rt {
        HW.need_resched(CpuId(cpu));
    }
}

/// Set RT priority on the currently running thread.
pub fn set_current_rt(enable: bool) {
    let cpu = percpu::cpu_id() as usize;
    let mut queue = SCHEDULER.lock_cpu(cpu);
    let current = queue.current_mut()
        .expect("set_current_rt: no current thread");
    current.is_rt = enable;
    current.rt_inherited = false;
}

/// Grant the current thread a transient RT boost (priority inheritance).
/// Called when it consumes data from a pipe an RT writer marked — see
/// `pipe::set_rt_boost_pending`.
pub fn boost_current_rt_inherited() {
    let cpu = percpu::cpu_id() as usize;
    let mut queue = SCHEDULER.lock_cpu(cpu);
    let current = queue.current_mut()
        .expect("boost_current_rt_inherited: no current thread");
    if !current.is_rt {
        current.is_rt = true;
        current.rt_inherited = true;
    }
}

/// One full pass over every scheduler container, removing `id` if parked.
/// The blocked pool is held across the queue scans (pool→queue is the
/// documented lock order), pinning wake-side pool removals for the pass.
enum ScanResult {
    Removed(TaskCtx),
    /// Currently `current` on the given CPU — cannot be yanked mid-execution.
    InFlight(u32),
    Absent,
}

fn scan_remove(id: TaskId) -> ScanResult {
    let mut pool = SCHEDULER.blocked.lock_unwrap();
    // A retired task can never consume a wake aimed at its park window —
    // discard both forms. Its registration dies with it: retire yanks the ctx
    // out of the outgoing slot, so park_outgoing never runs to clear it.
    pool.pending_wakes.remove(&id);
    pool.prepared.remove(&id);
    if let Some(ctx) = pool.remove_task(id) {
        // Blocked threads are not counted runnable — no refcount change.
        return ScanResult::Removed(ctx);
    }
    for i in 0..crate::arch::smp::cpu_count() as usize {
        let mut q = SCHEDULER.lock_cpu(i);
        if let Some(ctx) = q.remove_ready(id) {
            drop(q);
            drop(pool);
            SCHEDULER.leave_runnable(id.0);
            return ScanResult::Removed(ctx);
        }
        if q.outgoing_id() == Some(id) {
            let (mut ctx, _reason) = q.take_outgoing().unwrap();
            // The parked RSP still lives in the queue's save_rsp slot until
            // handle_outgoing files it — complete the ctx before handing it
            // out. handle_outgoing tolerates the now-empty slot.
            ctx.kernel_rsp = q.save_rsp();
            drop(q);
            drop(pool);
            // Outgoing tasks are still counted runnable for every reason:
            // Yield keeps the count, Block/Exit would only decrement later
            // in handle_outgoing/park_outgoing, which no longer run for it.
            SCHEDULER.leave_runnable(id.0);
            return ScanResult::Removed(ctx);
        }
        if q.current().is_some_and(|c| c.id == id) {
            return ScanResult::InFlight(i as u32);
        }
    }
    ScanResult::Absent
}

/// Remove a thread from the scheduler with proof of absence: when this
/// returns, `id` is not queued, not blocked, not parked in an outgoing slot,
/// not mid-steal, and not running on any CPU — and it can never reappear
/// (kill mark + insertion-point drops). Returns the ctx if this call removed
/// it, or None if the thread already left on its own (its ctx was dropped by
/// the exit path or an insertion point; its accounting is lost in that case).
///
/// Blocks until in-flight executions reach a scheduling boundary. Must not be
/// called for the calling thread itself, and the caller must not hold locks
/// the target could be spinning on with none of its own held (the target
/// only parks once it can make progress).
pub fn retire_task(id: TaskId) -> Option<TaskCtx> {
    if let (Some(pid), Some(tid)) = (percpu::current_pid(), percpu::current_tid()) {
        assert!(TaskId(pid, tid) != id, "retire_task: cannot retire self");
    }
    mark_killed(id);
    let deadline = crate::hw::now_ns() + 1_000_000_000;
    loop {
        match scan_remove(id) {
            ScanResult::Removed(ctx) => {
                clear_killed(id);
                return Some(ctx);
            }
            ScanResult::Absent => {
                // Trust "gone" only while no ctx is mid-hop on a stack: a
                // ctx popped before our mark could otherwise complete its
                // insert after we unmark. (Any insert while marked
                // self-drops under the destination's lock.)
                if CTX_TRANSITS.load(Ordering::SeqCst) == 0 {
                    clear_killed(id);
                    return None;
                }
            }
            ScanResult::InFlight(cpu) => {
                // Running right now — ask it for its next safe point, where
                // the kill mark drops it (park/insert/pick). Spec §7.6: this
                // is the one case the core needs `need_resched` for, and it
                // is already the one case the old scheduler needs it for.
                HW.need_resched(CpuId(cpu));
            }
        }
        if crate::hw::now_ns() > deadline {
            panic!("retire_task: {id} still in flight after 1s");
        }
        yield_now();
    }
}

pub fn current_address_space() -> Option<PageTables> {
    let cpu = percpu::cpu_id() as usize;
    let q = SCHEDULER.lock_cpu(cpu);
    q.current().and_then(|ctx| ctx.address_space.clone())
}

/// Block on a futex word unless it already changed. Returns whether it parked.
///
/// Registering before reading the word is the whole protocol: a `futex_wake`
/// that runs after the registration either marks the ticket or finds the
/// waiter parked, and one that ran before it stored the new value before
/// taking the pool lock this registration passed through — so the read below
/// sees it. A futex has no readiness to re-derive after the fact, which is
/// what the wake-generation counter and its lock existed to work around.
pub fn futex_wait(phys_addr: DirectMap, expected: u32, deadline: u64) -> bool {
    let queue = WaitQueue::futex(phys_addr);
    let ticket = queue.prepare_wait();
    if unsafe { *phys_addr.as_ptr::<u32>() } != expected {
        ticket.cancel();
        return false;
    }
    block_on(ticket, deadline);
    true
}

pub fn futex_wake(phys_addr: DirectMap, count: usize) -> u64 {
    let _transit = TransitGuard::new();
    let mut batch = WokenBatch::new();
    let n = {
        let mut pool = SCHEDULER.blocked.lock_unwrap();
        pool.take_by_event_limited(&EventSource::Futex(phys_addr), count, &mut batch) as u64
    };
    if !batch.is_empty() {
        SCHEDULER.enqueue_batch(batch);
    }
    n
}

pub fn with_current_ctx<R>(f: impl FnOnce(&TaskCtx) -> R) -> Option<R> {
    let cpu = percpu::cpu_id() as usize;
    let q = SCHEDULER.lock_cpu(cpu);
    q.current().map(f)
}

/// Flush the current thread's blocked/runqueue stats into ProcessData.
/// Called from teardown_resources while the thread is still current.
pub fn flush_current_stats(acct: &mut process::ProcessAccounting) {
    let cpu = percpu::cpu_id() as usize;
    let q = SCHEDULER.lock_cpu(cpu);
    if let Some(ctx) = q.current() {
        ctx.accounting.merge_into(acct);
    }
}

pub fn task_sched_state(id: TaskId) -> u8 {
    for i in 0..crate::arch::smp::cpu_count() as usize {
        if let Some(q) = SCHEDULER.try_lock_cpu(i) {
            if let Some(ctx) = q.current() {
                if ctx.id == id { return 0; }
            }
            if q.is_ready(id) { return 1; }
        }
    }
    if SCHEDULER.blocked.lock_unwrap().threads.contains_key(&id) {
        return 2;
    }
    3
}

/// Cumulative CPU time for a thread, wherever it currently lives. Running
/// threads include the live slice since they were scheduled; ready, outgoing,
/// and blocked threads report their accumulated total (charging happens on
/// every deschedule in do_schedule's take_current, so the stored value is
/// current). The scan must cover ALL containers with blocking locks: the old
/// current-plus-pool try_lock version reported 0 for any preempt-requeued
/// thread — which on a single core is every compute-bound thread whenever
/// the reader itself is running.
pub fn task_cpu_ns(id: TaskId) -> u64 {
    for i in 0..crate::arch::smp::cpu_count() as usize {
        let q = SCHEDULER.lock_cpu(i);
        if let Some(ns) = q.find_cpu_ns(id) {
            return ns;
        }
    }
    let pool = SCHEDULER.blocked.lock_unwrap();
    if let Some(ctx) = pool.threads.get(&id) {
        return ctx.cpu_ns();
    }
    0
}

/// Tail of a `do_schedule` frame for fresh-thread trampolines
/// (`process_start`/`thread_start`): release the CPU queue lock held across
/// `context_switch`, park the outgoing thread, and clear the in-schedule
/// re-entry guard that the switching CPU's `do_schedule` entry set. The
/// guard must be cleared AFTER `handle_outgoing` — it is what suppresses a
/// nested preempt while the outgoing thread is still being parked.
pub fn finish_fresh_thread_switch() {
    let cpu = percpu::cpu_id() as usize;
    vsched!("sched force_unlock cpus[{}]", cpu);
    unsafe { SCHEDULER.cpus[cpu].force_unlock(); }
    handle_outgoing();
    leave_schedule();
}

// ---------------------------------------------------------------------------
// Core scheduling logic
// ---------------------------------------------------------------------------

/// Scratch buffers for `drain_events` — per-CPU statics instead of ~2.7KB
/// stack frames in the scheduler's deepest call path.
struct DrainBuf(UnsafeCell<[EventSource; EVENT_QUEUE_SIZE]>);

// SAFETY: each CPU touches only its own slot, and drain_events runs only
// from scheduler context (do_schedule entry / idle loop) — never nested
// (IN_SCHEDULE guard) and never from interrupt context.
unsafe impl Sync for DrainBuf {}

static DRAIN_BUFS: [DrainBuf; MAX_CPUS] =
    [const { DrainBuf(UnsafeCell::new([EventSource::Keyboard; EVENT_QUEUE_SIZE])) }; MAX_CPUS];

/// Drain per-CPU event queue and wake affected threads. One lock acquisition.
/// Process pending events and expired deadlines. Returns the next deadline
/// (absolute nanos_since_boot), or 0 if no threads have deadlines.
fn drain_events() -> u64 {
    // Consume this CPU's pending irq_ring records (scheduler-core spec §11
    // Stage 2). Each ISR stamped its record at IRQ time and forced this
    // scheduler entry via need_resched; records only exist on the CPU that
    // took the interrupt, so no cpu gates are needed.
    //
    // xHCI (keyboard/mouse): controller poll → HID dispatch_report →
    // EventSource pushes via push_event.
    crate::drivers::xhci::poll_if_pending();

    // Virtio-net: convert the record into waiter wakes and io_uring CQEs.
    if crate::irq_ring::take(crate::irq_ring::IrqSource::Net).is_some() {
        PERCPU_EVENTS[percpu::cpu_id() as usize].push(EventSource::Network);
        let watchers = crate::net::io_uring_watchers();
        if !watchers.is_empty() {
            crate::io_uring::complete_pending_for_event(
                &watchers,
                EventSource::Network,
            );
        }
    }

    // Virtio-sound: the ISR already drained the used ring and pushed
    // timestamped completion records (the audio DATA path in crate::audio);
    // the irq_ring record only drives wakes and io_uring CQEs.
    if crate::irq_ring::take(crate::irq_ring::IrqSource::Audio).is_some() {
        PERCPU_EVENTS[percpu::cpu_id() as usize].push(EventSource::Audio);
        let watchers = crate::audio::io_uring_watchers();
        if !watchers.is_empty() {
            crate::io_uring::complete_pending_for_event(
                &watchers,
                EventSource::Audio,
            );
        }
    }

    let cpu = percpu::cpu_id() as usize;

    let overflow = PERCPU_EVENTS[cpu].overflow_count.swap(0, Ordering::Relaxed);
    if overflow > 0 {
        crate::log!("EVENT QUEUE OVERFLOW: cpu={} dropped={} events", cpu, overflow);
    }

    // SAFETY: exclusive per-CPU access — see DrainBuf.
    let events = unsafe { &mut *DRAIN_BUFS[cpu].0.get() };
    let mut event_count = 0usize;
    PERCPU_EVENTS[cpu].drain_into(events, &mut event_count);

    let _transit = TransitGuard::new();
    let mut batch = WokenBatch::new();
    {
        let mut pool = SCHEDULER.blocked.lock_unwrap();
        for i in 0..event_count {
            pool.take_by_event_into(&events[i], &mut batch);
        }
    }
    if overflow > 0 {
        // Dropped events would strand their waiters — re-derive readiness
        // for every waited-on source so no wake is lost. Readiness runs
        // WITHOUT the pool lock: event_ready takes subsystem locks (pipe,
        // listener, keyboard, ...), and holding the pool across them would
        // invert the subsystem→pool order used by paths like the keyboard
        // handler's dump_blocked into an AB-BA deadlock. A source becoming
        // ready between snapshot and re-take is delivered by its own
        // event/interrupt path as usual.
        let waited: Vec<EventSource> = SCHEDULER.blocked.lock_unwrap()
            .by_event.keys().copied().collect();
        let ready: Vec<EventSource> = waited.into_iter()
            .filter(crate::waitq::source_ready)
            .collect();
        if !ready.is_empty() {
            let mut pool = SCHEDULER.blocked.lock_unwrap();
            for event in &ready {
                pool.take_by_event_into(event, &mut batch);
            }
        }
    }
    let next_deadline = {
        let mut pool = SCHEDULER.blocked.lock_unwrap();
        check_deadlines_locked(&mut pool, &mut batch);
        pool.deadlines.first_key_value()
            .map(|(&(dl, _), _)| dl).unwrap_or(0)
    };
    if !batch.is_empty() {
        SCHEDULER.enqueue_batch(batch);
    }
    next_deadline
}

/// Source of the outgoing RSP slot for `run_task_on_self`.
enum RspSource {
    /// `do_schedule` path — write into the queue's `save_rsp` slot.
    Saved(*mut u64),
    /// `cpu_idle_loop` path — write into the per-CPU idle RSP slot.
    Idle,
}

/// Switch to `new` on the current CPU.
///
/// `min_vruntime` is the frontier from which non-runnable processes' lag is
/// measured — advanced at every pick (see `toyos_sched::fair::Frontier`).
///
/// Lock protocol: `queue.into_raw()` leaks the guard so the lock stays held
/// across `context_switch`. The resuming task is responsible for calling
/// `force_unlock` on the per-CPU queue and then `handle_outgoing()`.
fn run_task_on_self(
    mut queue: CpuQueueGuard<'_>,
    vrt: u64,
    new: TaskCtx,
    next_deadline: u64,
    old_rsp: RspSource,
) {
    // No is_killed assert here: a kill mark may legally land between
    // pick_next's filter and this point — that in-flight execution is
    // exactly what retire_task waits out. Poison cannot race the same way
    // (it only ever targets the poisoning CPU's own current task, which is
    // never simultaneously in a ready queue).
    assert!(!is_poisoned(new.id),
        "run_task_on_self: scheduling a poisoned task {}", new.id);
    SCHEDULER.min_vruntime.advance(vrt);
    // One clock sample for the whole dispatch — the quantum, the timer
    // deadline, the trace record and the incoming task's charge all date from
    // the same instant. Spec §6.2's "sampled ONCE per pass"; this path used
    // to read the TSC three times and let the three disagree.
    let now_pick = crate::hw::now_ns();
    let quantum = if next_deadline > now_pick {
        QUANTUM_NS.min(next_deadline - now_pick)
    } else {
        QUANTUM_NS
    };
    // The task key names the *incoming* thread. `percpu::current_tid` is
    // still the outgoing one here — it is updated below — so the old
    // ambient-read trace recorded whoever was leaving.
    HW.trace(TraceEvent {
        ts: Nanos(now_pick),
        cpu: CpuId(percpu::cpu_id()),
        kind: TraceKind::Schedule { task: toyos_sched::task::TaskKey(new.id.pack()) },
    });
    HW.set_timer(Nanos(now_pick).after(quantum));
    let new_cr3 = new.cr3();
    let new_fs_base = new.fs_base;
    let new_ks_top = new.kernel_stack_top();
    let new_rsp = new.kernel_rsp;
    let new_tid = new.id.1;
    let new_pid = new.id.0;

    let mut new = new;
    // Deliberately a *fresh* sample, not `now_pick`. `scheduled_at` opens the
    // interval the task is charged for, and everything between `now_pick` and
    // here is the scheduler's own arming work — three x2APIC MSR writes, each
    // an exit to the device model under TCG. Dating the charge from
    // `now_pick` bills that per *dispatch*, so it lands on whichever task is
    // dispatched most often; measured, that is soundd, and it cost 21% of its
    // wakes on `audio_tone_load` at smp=1. One clock sample per pass is the
    // stage 7 shape, and it is right there because the pass charges at entry —
    // not because the timestamps can be shared with the arming path.
    new.start_cpu_timer(crate::hw::now_ns());
    new.last_cpu = Some(percpu::cpu_id());
    queue.set_current(new);

    let old_rsp_ptr = match old_rsp {
        RspSource::Saved(p) => p,
        RspSource::Idle => percpu::idle_rsp_ptr(),
    };

    percpu::set_current_tid(Some(new_tid));
    percpu::set_current_pid(Some(new_pid));
    unsafe { percpu::set_kernel_stack(new_ks_top); }
    unsafe { new_cr3.activate(); }
    cpu::wrfsbase(new_fs_base);

    vsched!("sched run_task_on_self pid={} tid={} new_rsp={:#x}", new_pid, new_tid, new_rsp);
    queue.into_raw();
    unsafe { context_switch(old_rsp_ptr, new_rsp); }
    let resume_cpu = percpu::cpu_id() as usize;
    vsched!("sched resume force_unlock cpus[{}]", resume_cpu);
    unsafe { SCHEDULER.cpus[resume_cpu].force_unlock(); }

    handle_outgoing();
}

fn do_schedule(reason: SwitchReason) {
    enter_schedule();
    vsched!("sched do_schedule reason={}", match &reason {
        SwitchReason::Yield => "Yield",
        SwitchReason::Block { .. } => "Block",
        SwitchReason::Exit => "Exit",
    });
    let next_deadline = drain_events();

    let cpu = percpu::cpu_id() as usize;
    let now = crate::hw::now_ns();

    let mut queue = SCHEDULER.lock_cpu(cpu);

    if let Some(mut old) = queue.take_current() {
        check_stack_canary(&old);
        old.fs_base = cpu::rdfsbase();
        let elapsed = if old.scheduled_at > 0 { now - old.scheduled_at } else { 0 };
        old.stop_cpu_timer(now);
        queue.charge(&SCHEDULER, old.id.0, elapsed);
        queue.set_outgoing(old, reason);
    }

    if let Some((vrt, new)) = queue.pick_next() {
        let old_rsp_ptr = queue.save_rsp_ptr();
        run_task_on_self(queue, vrt, new, next_deadline, RspSource::Saved(old_rsp_ptr));
        leave_schedule();
        return;
    }

    crate::trace::trace(crate::trace::Kind::SchedIdle, next_deadline as u32);
    let old_rsp_ptr = queue.save_rsp_ptr();
    percpu::set_current_tid(None);
    percpu::set_current_pid(None);
    unsafe { percpu::set_kernel_stack(percpu::idle_stack_top()); }
    unsafe { crate::mm::paging::kernel_cr3().activate(); }

    vsched!("sched do_schedule->idle ctx_switch to idle_rsp={:#x}", percpu::idle_rsp());
    queue.into_raw();
    unsafe { context_switch(old_rsp_ptr, percpu::idle_rsp()); }
    let resume_cpu = percpu::cpu_id() as usize;
    vsched!("sched resume(idle-path) force_unlock cpus[{}]", resume_cpu);
    unsafe { SCHEDULER.cpus[resume_cpu].force_unlock(); }

    handle_outgoing();
    leave_schedule();
}

/// Move the outgoing thread into the blocked pool, or decline the park when a
/// wake reached it in the window. Returns whether the thread held a
/// registration — a converted source needs no further recheck, an unconverted
/// one does (see `handle_outgoing`).
///
/// The other race the window opens is the LAPIC one-shot: this CPU's timer was
/// armed before the deadline existed in the pool, so re-arm if it is earlier.
fn park_outgoing(queue: CpuQueueGuard<'_>, mut old: TaskCtx, event: Option<EventSource>, deadline: u64) -> bool {
    old.blocked_on = event;
    old.deadline = deadline;
    let tag = event.as_ref()
        .map(crate::trace::event_source_tag)
        .unwrap_or(0xFF_000000);
    crate::trace::trace(crate::trace::Kind::Block, tag);
    let pid = old.id.0;
    drop(queue);

    let mut pool = SCHEDULER.blocked.lock_unwrap();
    let prepared = pool.prepared.remove(&old.id);
    // The kill mark is checked under the pool lock, adjacent to the insert:
    // a check before taking the lock would leave a gap where retire_task's
    // scan misses the ctx (not yet in the pool) while the mark misses the
    // park (checked too early).
    if is_killed(old.id) {
        // Retirement in progress — terminal hand-off (see KILLED).
        drop(pool);
        SCHEDULER.leave_runnable(pid);
        drop(old);
        return prepared.is_some();
    }
    if let Some(p) = &prepared {
        assert_eq!(p.source, event,
            "park_outgoing: {} parked on a source it did not register on", old.id);
    }
    // A wake landed between the decision to block and this insert: for a
    // registered source it marked the ticket, for the rest only wake_task's
    // sticky set can report it. The sticky entry is consumed either way —
    // left behind, it would fire again at the next park.
    let sticky = pool.pending_wakes.remove(&old.id);
    let woken_in_window = prepared.as_ref().is_some_and(|p| p.fired) || sticky;
    if woken_in_window {
        // Honor it now: stay runnable and requeue locally instead of parking.
        // Spurious from the blocker's view; all block paths retry in a loop.
        drop(pool);
        old.blocked_on = None;
        old.deadline = 0;
        let is_rt = old.is_rt;
        let vrt = SCHEDULER.current_runnable_vruntime(pid);
        let cpu = percpu::cpu_id() as usize;
        let mut q = SCHEDULER.lock_cpu(cpu);
        q.insert(vrt, old);
        drop(q);
        if is_rt {
            HW.need_resched(CpuId(cpu as u32));
        }
        return prepared.is_some();
    }
    SCHEDULER.leave_runnable(pid);
    pool.insert(old);
    drop(pool);
    if deadline > 0 {
        crate::arch::apic::ensure_armed_before(deadline);
    }
    prepared.is_some()
}

fn handle_outgoing() {
    let cpu = percpu::cpu_id() as usize;
    vsched!("sched handle_outgoing entry cpu={}", cpu);
    // The outgoing ctx lives on this stack from take_outgoing until it
    // reaches the pool, a queue, or is dropped — invisible to retire_task's
    // scan, so the whole frame counts as an in-flight hop (see CTX_TRANSITS).
    let _transit = TransitGuard::new();
    let mut queue = SCHEDULER.lock_cpu(cpu);
    let Some((mut old, reason)) = queue.take_outgoing() else { return };
    old.kernel_rsp = queue.save_rsp();
    vsched!("sched handle_outgoing took outgoing {} kernel_rsp={:#x}", old.id, old.kernel_rsp);
    // Inherited RT ends at the next scheduling boundary: blocking (the
    // normal hand-back) or quantum expiry/yield (bounds a boosted thread
    // that never blocks — it must not round-robin in the RT band forever).
    if old.rt_inherited {
        old.is_rt = false;
        old.rt_inherited = false;
    }
    match reason {
        SwitchReason::Yield => {
            // Yielding thread stays runnable — refcount unchanged.
            let vrt = SCHEDULER.current_runnable_vruntime(old.id.0);
            queue.insert(vrt, old);
        }
        SwitchReason::Block { event, deadline } => {
            let ticketed = park_outgoing(queue, old, event, deadline);
            // Unconverted sources (spec stage 5) have nothing to mark in the
            // park window, so their race is still covered the old way: a wake
            // consumed before the insert is recovered by re-deriving
            // readiness. Any variant left out of this recheck (PipeWritable
            // and Listener once were) turns the race into a permanent
            // writer/accept deadlock. Spurious wakes are fine, all blocking
            // paths retry in a loop.
            let raced = !ticketed && event.as_ref().is_some_and(crate::waitq::source_ready);
            if raced {
                wake_by_event(event.unwrap());
            }
        }
        SwitchReason::Exit => {
            let pid = old.id.0;
            drop(queue);
            SCHEDULER.leave_runnable(pid);
            // Terminal park: nothing aimed at this thread's park window can
            // ever be consumed — drop it so neither index grows unboundedly.
            // A live registration here means the thread died (panic recovery)
            // between prepare_wait and its park.
            let mut pool = SCHEDULER.blocked.lock_unwrap();
            pool.pending_wakes.remove(&old.id);
            pool.prepared.remove(&old.id);
            drop(pool);
            drop(old);
        }
    }
}

// ---------------------------------------------------------------------------
// Idle loop
// ---------------------------------------------------------------------------

static IDLE_HEALTH_COUNTER: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

fn cpu_idle_loop() -> ! {
    let idle_proof = unsafe { IdleProof::new_unchecked() };
    'idle: loop {
        // Health check every ~1000 idle iterations
        if IDLE_HEALTH_COUNTER.fetch_add(1, Ordering::Relaxed) % 1000 == 999 {
            log_health();
        }

        let next_deadline = drain_events();

        // Reap threads that died in panic recovery. This is their *only*
        // cleanup: `try_recover_from_panic` cannot touch the process table
        // (it may hold any lock the faulted thread held), so it hands the
        // thread over through the poison set and jumps straight here.
        // Fixed-size, so an idle iteration that reaps nothing allocates nothing.
        let mut wakes = [None; MAX_CPUS];
        {
            let mut guard = process::PROCESS_TABLE.lock();
            let table = guard.as_mut().unwrap();
            process::collect_orphan_zombies(table,idle_proof);

            for (slot, wake) in POISONED.iter().zip(wakes.iter_mut()) {
                let raw = slot.load(Ordering::Relaxed);
                if raw == u64::MAX { continue; }
                let id = TaskId::unpack(raw);
                *wake = process::zombify_poisoned(table, id.0, id.1);
                // Before the wake below: `wake_task` drops wakes aimed at a
                // poisoned task, and the waiter may be poisoned itself.
                clear_poison(id);
            }
        }
        for (pid, tid) in wakes.into_iter().flatten() {
            wake_task(TaskId(pid, tid));
        }

        let cpu = percpu::cpu_id() as usize;
        {
            // Own queue stays locked across the pick AND the steal probe: a
            // stolen ctx moves from the sibling's queue straight into ours
            // (as `current`) without ever being outside every lock.
            // retire_task relies on this — a task it cannot find in any
            // container is provably not hiding on an idle CPU's stack.
            //
            // Lock order: own queue (blocking, we are its only long holder)
            // then sibling queues (try_lock only). Two idle CPUs probing
            // each other skip instead of deadlocking.
            let mut queue = SCHEDULER.lock_cpu(cpu);
            let mut next = queue.pick_next();
            if next.is_none() {
                // Steal from ANY sibling with queued work. Deliberately no
                // "sibling must also have a current task" filter: a ready
                // task stranded on a sleeping CPU's queue would otherwise be
                // invisible to recovery forever — every CPU halts and the
                // task never runs again.
                //
                // Vruntime portability: vruntime lives in
                // `Scheduler::sched_state` (Pid-keyed, global), so the
                // (vrt, TaskId) key is valid on any CPU's queue. Stealing
                // is a pure move — no renormalization.
                let count = crate::arch::smp::cpu_count() as usize;
                for other in (0..count).filter(|&c| c != cpu) {
                    let Some(mut other_q) = SCHEDULER.try_lock_cpu(other) else { continue };
                    if other_q.ready_len() == 0 { continue; }
                    if let Some((vrt, ctx)) = other_q.pick_next() {
                        vsched!("sched steal cpus[{}] -> {}", other, ctx.id);
                        next = Some((vrt, ctx));
                        break;
                    }
                }
            }
            if let Some((vrt, new)) = next {
                run_task_on_self(queue, vrt, new, next_deadline, RspSource::Idle);
                continue 'idle;
            }
        }

        // No work to run on this CPU — flush buffered log output to the
        // serial backend before sleeping. Draining here (and not from the
        // log!() fast path) is what keeps log!() from blocking under
        // scheduler locks. One chunk per BackendGuard acquisition: the guard
        // holds interrupts off for its lifetime, so a full-ring drain under
        // one guard would block interrupts for up to 64KiB of serial I/O.
        // Dropping the guard between chunks bounds each IRQs-off window.
        loop {
            let mut backend = crate::drivers::serial::BackendGuard::lock();
            if crate::drivers::log_ring::drain_chunk_to_serial(&mut backend) == 0 {
                break;
            }
        }

        // Idle: arm one-shot timer for next deadline, or stop if none.
        // The CPU will sleep until a timer or MSI-X interrupt arrives.
        if next_deadline > 0 {
            if next_deadline <= crate::hw::now_ns() {
                continue; // deadline already expired, re-check
            }
            HW.set_timer(Nanos(next_deadline));
        } else {
            HW.stop_timer();
        }

        // Final re-check with IRQs disabled, immediately before sti;hlt.
        // A wake that landed after the pick attempt above (enqueue + kick
        // IPI delivered while we were still awake) would otherwise be
        // consumed as an ordinary interrupt and then slept through forever.
        // With IF=0, anything that arrives from here on stays pending and
        // terminates the hlt; anything that arrived before is visible to
        // these checks.
        //
        // Not `Machine::irq_guard`, and the reason is worth keeping: that
        // guard restores the caller's IF, and both exits from here must
        // *set* it. The panic path `cli`s and lands in this loop through
        // `schedule_no_return`, so the entering IF is genuinely sometimes 0 —
        // restoring it would strand the recovering CPU. The halt exit has a
        // second reason: `sti` and `hlt` must be one instruction pair (STI
        // shadow), which no guard drop can be.
        unsafe { core::arch::asm!("cli", options(nomem, nostack)); }
        let work_pending = {
            let queue = SCHEDULER.lock_cpu(cpu);
            queue.ready_len() > 0
        } || PERCPU_EVENTS[cpu].has_events()
            || crate::irq_ring::any_pending_self();
        if work_pending {
            unsafe { core::arch::asm!("sti", options(nomem, nostack)); }
            continue;
        }
        HW.trace(TraceEvent {
            ts: Nanos(crate::hw::now_ns()),
            cpu: CpuId(cpu as u32),
            kind: TraceKind::IdleEnter,
        });
        HW.halt();
        HW.trace(TraceEvent {
            ts: Nanos(crate::hw::now_ns()),
            cpu: CpuId(cpu as u32),
            kind: TraceKind::IdleExit,
        });
    }
}

// ---------------------------------------------------------------------------
// Stack canary — detects kernel stack overflow on context switch
// ---------------------------------------------------------------------------

const STACK_CANARY: u64 = 0xDEAD_BEEF_CAFE_BABE;

pub fn write_stack_canary(stack: &OwnedAlloc) {
    unsafe { *(stack.ptr() as *mut u64) = STACK_CANARY; }
}

fn check_stack_canary(ctx: &TaskCtx) {
    let canary = unsafe { *(ctx.kernel_stack.ptr() as *const u64) };
    if canary != STACK_CANARY {
        panic!("KERNEL STACK OVERFLOW: tid={} canary={:#x} expected={:#x}",
            ctx.id.1, canary, STACK_CANARY);
    }
}

// ---------------------------------------------------------------------------
// Blocked thread dump — diagnostic for "system hangs" debugging
// ---------------------------------------------------------------------------

fn event_name(event: &EventSource) -> &'static str {
    match event {
        EventSource::Keyboard => "Keyboard",
        EventSource::Mouse => "Mouse",
        EventSource::Network => "Network",
        EventSource::Listener(_) => "Listener",
        EventSource::PipeReadable(_) => "PipeR",
        EventSource::PipeWritable(_) => "PipeW",
        EventSource::Audio => "Audio",
        EventSource::Futex(_) => "Futex",
        EventSource::IoUring(_) => "IoUring",
    }
}

/// Dump all blocked threads with their registered events and deadlines.
/// Safe to call from any context: try_lock everywhere — this runs from
/// diagnostic paths (keyboard hotkey) that may already hold subsystem locks,
/// and a blocking pool acquire there would complete an AB-BA cycle with
/// anyone holding the pool while touching subsystems.
pub fn dump_blocked() {
    let Some(guard) = SCHEDULER.blocked.try_lock() else {
        crate::log!("dump_blocked: pool busy, skipped");
        return;
    };
    let Some(pool) = guard.as_ref() else { return };
    let now = crate::hw::now_ns();
    crate::log!("=== BLOCKED THREADS ({}) ===", pool.threads.len());
    for (&id, ctx) in &pool.threads {
        let (pid, tid) = (id.0, id.1);
        let since_ms = if ctx.blocked_since > 0 { (now - ctx.blocked_since) / 1_000_000 } else { 0 };

        let events = match &ctx.blocked_on {
            Some(e) => event_name(e),
            None => "(none)",
        };

        // Try to get process name without blocking
        let mut name_buf = [0u8; 28];
        let mut got_name = false;
        if let Some(guard) = crate::process::PROCESS_TABLE.try_lock() {
            if let Some(table) = guard.as_ref() {
                if let Some(proc) = table.get(ctx.id.0) {
                    name_buf = *proc.name();
                    got_name = true;
                }
            }
        }
        let name = if got_name {
            core::str::from_utf8(&name_buf).unwrap_or("?").trim_end_matches('\0')
        } else {
            "?"
        };

        if ctx.deadline > 0 {
            let dl_secs = ctx.deadline / 1_000_000_000;
            let dl_ms = (ctx.deadline % 1_000_000_000) / 1_000_000;
            crate::log!("  pid={} tid={} ({}) events=[{}] deadline={}.{:03}s since={}ms",
                pid, tid, name, events, dl_secs, dl_ms, since_ms);
        } else {
            crate::log!("  pid={} tid={} ({}) events=[{}] deadline=none since={}ms",
                pid, tid, name, events, since_ms);
        }
    }
    crate::log!("=== END BLOCKED ===");
}

// ---------------------------------------------------------------------------
// Context switch (naked asm, unchanged)
// ---------------------------------------------------------------------------

#[unsafe(naked)]
unsafe extern "C" fn context_switch(old_rsp: *mut u64, new_rsp: u64) {
    naked_asm!(
        "pushfq",
        "push rbp",
        "push rbx",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "mov [rdi], rsp",   // save old RSP
        "mov rsp, rsi",     // load new RSP
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
