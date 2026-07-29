//! What the kernel attaches to a task, and the two environment-supplied
//! pieces the core crate refuses to implement itself (spec §10.1): the leaf
//! lock and the saved-context type.
//!
//! The split between [`KernelCtx`] and [`KernelPayload`] is not cosmetic. The
//! context switch reaches a task through a raw `*mut X::Ctx` and nothing else,
//! so everything the switch needs — rsp, cr3, fs_base, the kernel stack top,
//! the identity to publish into percpu — lives in the context. Everything the
//! *kernel* needs about a live task and must release exactly once lives in the
//! payload, which only `Hw::release` ever consumes.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

use toyos_sched::fair::{FairShare, ShareState};
use toyos_sched::hw::Nanos;
use toyos_sched::msg::Msg;
use toyos_sched::sync::LeafLock;
use toyos_sched::task::{SchedPayload, TaskAccounting, TaskShared, WaitClass};
use toyos_sched::waitq::{WaitList, WaitQueue, WaitTicket};

use crate::mm::paging::Cr3;
use crate::process::{OwnedAlloc, PageTables, ProcessAccounting, TaskId};
use crate::sync::Lock;

/// The environment's small shared cell. The kernel's `Lock` already raises the
/// preempt count for its whole lifetime, which is exactly the property spec
/// §7.2's N3 needs of a mailbox producer — so a wake path that holds one is
/// automatically a legal producer.
pub struct KernelLock<T>(Lock<T>);

impl<T> KernelLock<T> {
    pub const fn new(value: T) -> Self {
        Self(Lock::new(value))
    }
}

impl<T: Send> LeafLock<T> for KernelLock<T> {
    fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        f(&mut self.0.lock())
    }
}

pub type KMsg = Msg<KernelPayload>;
pub type KShared = TaskShared<KMsg>;
pub type KShare = FairShare<KernelLock<ShareState>>;
pub type KWaitList = KernelLock<WaitList<KMsg>>;
pub type KWaitQueue = WaitQueue<KMsg, KWaitList>;
/// A wait ticket in the kernel's one instantiation of the two-phase commit.
/// Named here so blocking sites do not have to spell the generics.
pub type Ticket<'q> = WaitTicket<'q, KMsg, KWaitList>;

/// A queue in a `static` — the device queues and the futex/park buckets.
pub const fn static_queue(class: WaitClass) -> KWaitQueue {
    KWaitQueue::new(class, KernelLock::new(WaitList::new()))
}

pub fn heap_queue(class: WaitClass) -> Arc<KWaitQueue> {
    Arc::new(static_queue(class))
}

/// The saved callee context, plus everything `Hw::switch` must load without
/// dereferencing anything else.
pub struct KernelCtx {
    /// Saved kernel stack pointer, written by the `context_switch` asm.
    pub rsp: u64,
    pub cr3: Cr3,
    pub fs_base: u64,
    pub kernel_stack_top: u64,
    /// `None` is this CPU's idle context.
    pub id: Option<TaskId>,
}

/// Everything the kernel owns per task and must release exactly once. The
/// address-space `Arc` in here is the crash.md double-drop: it is handed back
/// by `DeadTask::finalize`, which consumes the only owner, so it cannot be
/// released twice.
pub struct KernelPayload {
    pub id: TaskId,
    pub kernel_stack: OwnedAlloc,
    pub address_space: Option<PageTables>,
    /// The cross-CPU-readable face of this task. A `CpuSched` is `!Sync`, so a
    /// remote `ps` cannot walk it; what it can read is here.
    pub handle: Arc<TaskHandle>,
}

impl SchedPayload for KernelPayload {
    type Ctx = KernelCtx;
    type ShareLock = KernelLock<ShareState>;
}

/// State word values for `task_sched_state` (the `ps` column).
pub const SCHED_RUNNING: u8 = 0;
pub const SCHED_READY: u8 = 1;
pub const SCHED_BLOCKED: u8 = 2;
pub const SCHED_UNKNOWN: u8 = 3;

/// What a thread other than the one running can be asked about.
///
/// Under the old scheduler `ps` walked every CPU's queue under its lock.
/// Per-CPU exclusive ownership makes that unwritable — a `CpuSched` is `!Sync`
/// and reachable only from its own CPU. What a remote reader actually wants is
/// published here instead, by the owning CPU, at each end of a pass.
pub struct TaskHandle {
    cpu_ns: AtomicU64,
    /// Dispatch timestamp while running, 0 otherwise. A reader adds the live
    /// slice itself, so a running thread's CPU time does not stand still
    /// between passes.
    running_since: AtomicU64,
    acct: Lock<TaskAccounting>,
}

impl TaskHandle {
    pub fn new() -> Self {
        Self {
            cpu_ns: AtomicU64::new(0),
            running_since: AtomicU64::new(0),
            acct: Lock::new(TaskAccounting::default()),
        }
    }

    pub(crate) fn publish(&self, acct: &TaskAccounting, running_since: Option<Nanos>) {
        self.cpu_ns.store(acct.cpu_ns, Ordering::Relaxed);
        self.running_since
            .store(running_since.map_or(0, |n| n.0), Ordering::Relaxed);
    }

    /// Called once, by `Hw::release`, with the accounting the linear value
    /// carried. From here on the thread's numbers are frozen.
    pub(crate) fn finalize(&self, acct: TaskAccounting) {
        self.cpu_ns.store(acct.cpu_ns, Ordering::Relaxed);
        self.running_since.store(0, Ordering::Relaxed);
        *self.acct.lock() = acct;
    }

    pub fn cpu_ns(&self) -> u64 {
        let base = self.cpu_ns.load(Ordering::Relaxed);
        match self.running_since.load(Ordering::Relaxed) {
            0 => base,
            since => base + crate::hw::now_ns().saturating_sub(since),
        }
    }

    pub fn merge_into(&self, target: &mut ProcessAccounting) {
        let acct = self.acct.lock();
        merge_accounting(&acct, target);
    }
}

/// A thread's two scheduler-visible faces, kept by the process table.
///
/// They are separate values because they are created at different instants:
/// the counters exist before the task record does (the payload owns them), and
/// the rendezvous word is minted by `TaskBuilder::build`.
#[derive(Clone)]
pub struct ThreadSched {
    pub handle: Arc<TaskHandle>,
    pub shared: Arc<KShared>,
}

impl ThreadSched {
    pub fn sched_state(&self) -> u8 {
        use toyos_sched::task::TaskState;
        match self.shared.state() {
            TaskState::Running(_) => SCHED_RUNNING,
            TaskState::Ready(_) | TaskState::WakeQueued(_) | TaskState::InTransit(_) => SCHED_READY,
            TaskState::Blocked(_) | TaskState::Committing(..) => SCHED_BLOCKED,
            TaskState::Dead => SCHED_UNKNOWN,
        }
    }
}

/// The core's per-class blocked-time array, spread over the kernel's named
/// counters. One place, so the class order is stated once.
pub fn merge_accounting(acct: &TaskAccounting, target: &mut ProcessAccounting) {
    target.blocked_io_ns += acct.blocked_ns[WaitClass::Io.index()];
    target.blocked_futex_ns += acct.blocked_ns[WaitClass::Futex.index()];
    target.blocked_pipe_ns += acct.blocked_ns[WaitClass::Pipe.index()];
    target.blocked_ipc_ns += acct.blocked_ns[WaitClass::Ipc.index()];
    target.blocked_other_ns += acct.blocked_ns[WaitClass::Other.index()];
    target.runqueue_wait_ns += acct.runqueue_wait_ns;
}
