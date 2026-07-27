//! Scaffolding shared by the loom models: the message type, the modelled
//! preempt count, the leaf lock and the kick recorder.

use loom::sync::atomic::{AtomicUsize, Ordering};
use loom::sync::{Mutex, MutexGuard};

use crate::hw::{CpuId, Kicker};
use crate::mailbox::{PreemptGuard, SchedMsg};
use crate::task::{TaskKey, WakeCause, WakeReason};
use crate::waitq::{LeafLock, WaitList};

pub const CPU0: CpuId = CpuId(0);
pub const CPU1: CpuId = CpuId(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Msg {
    Wake(TaskKey, WakeReason),
    Retire(TaskKey),
    /// Stands in for the ownership-carrying `Adopt`/`StealRequest` traffic
    /// that Stage 4 adds; the mailbox models only need distinguishable
    /// payloads.
    Probe(u32),
}

impl SchedMsg for Msg {
    fn wake(key: TaskKey, cause: WakeCause) -> Self {
        Msg::Wake(key, cause.reason)
    }

    fn retire(key: TaskKey) -> Self {
        Msg::Retire(key)
    }
}

/// The modelled preempt count of one CPU.
///
/// A preempt-disabled region and that CPU's IRQ-exit scheduler pass are
/// mutually exclusive — which is precisely what the kernel's preempt count
/// buys: an IRQ may interrupt the region, but its exit path refuses to run a
/// pass while the count is nonzero, so the interrupted context resumes and
/// finishes what it started.
///
/// With `--features no-preempt-guard` the guard is **modelled away**:
/// [`PreemptModel::disable`] returns a guard that excludes nothing, so the
/// pass may run inside a half-finished push. That is the forbidden
/// interleaving of spec §7.2, and `tests/loom_mailbox.rs` asserts it is
/// detected.
pub struct PreemptModel {
    section: Mutex<()>,
}

impl PreemptModel {
    pub fn new() -> Self {
        Self {
            section: Mutex::new(()),
        }
    }

    pub fn disable(&self) -> ThreadGuard<'_> {
        if cfg!(feature = "no-preempt-guard") {
            ThreadGuard(None)
        } else {
            ThreadGuard(Some(self.section.lock().unwrap()))
        }
    }

    /// IRQ exit: may a scheduler pass run here? `None` means preemption is
    /// disabled in the interrupted context.
    pub fn enter_pass(&self) -> Option<PassGuard<'_>> {
        self.section.try_lock().ok().map(PassGuard)
    }
}

impl Default for PreemptModel {
    fn default() -> Self {
        Self::new()
    }
}

/// Thread-context guard: what every producer must hold to push (N3).
pub struct ThreadGuard<'a>(#[allow(dead_code)] Option<MutexGuard<'a, ()>>);

// SAFETY: the guard owns the modelled CPU's preempt-disabled section, so the
// pushing context cannot be descheduled while it lives — except under
// `--features no-preempt-guard`, where the model deliberately lies in order
// to prove that the lie is caught.
#[allow(unsafe_code)]
unsafe impl PreemptGuard for ThreadGuard<'_> {}

/// The scheduler pass's exclusion against preempt-disabled sections. A pass
/// runs with preemption disabled itself (spec §6.2), so it is also what the
/// consumer's stub re-push pushes under.
pub struct PassGuard<'a>(#[allow(dead_code)] MutexGuard<'a, ()>);

// SAFETY: a pass owns the CPU's preempt-disabled section for its duration.
#[allow(unsafe_code)]
unsafe impl PreemptGuard for PassGuard<'_> {}

/// IRQ context: not preemptible by construction, so it needs no exclusion.
pub struct IrqGuard;

// SAFETY: an interrupt handler cannot be preempted (spec §7.2).
#[allow(unsafe_code)]
unsafe impl PreemptGuard for IrqGuard {}

/// A remote CPU's thread context: it has its own preempt count, which this
/// CPU's model does not track.
pub struct RemoteGuard;

// SAFETY: models a preempt-disabled region on another CPU; the exclusion that
// matters here is the local one.
#[allow(unsafe_code)]
unsafe impl PreemptGuard for RemoteGuard {}

/// `LeafLock` over loom's mutex, so the wait-queue models exercise the real
/// critical sections.
pub struct LoomLock<T>(Mutex<T>);

impl<T> LoomLock<T> {
    pub fn new(value: T) -> Self {
        Self(Mutex::new(value))
    }
}

impl<T: Send> LeafLock<T> for LoomLock<T> {
    fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        f(&mut self.0.lock().unwrap())
    }
}

pub fn wait_list<M>() -> LoomLock<WaitList<M>>
where
    WaitList<M>: Send,
{
    LoomLock::new(WaitList::new())
}

/// Counts targeted IPIs. In these models an IPI's only observable effect is
/// "the target cannot stay halted", which is what the sleep model asserts.
#[derive(Default)]
pub struct Kicks {
    count: AtomicUsize,
}

impl Kicks {
    pub fn new() -> Self {
        Self {
            count: AtomicUsize::new(0),
        }
    }

    pub fn count(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }
}

impl Kicker for Kicks {
    fn kick(&self, _target: CpuId) {
        self.count.fetch_add(1, Ordering::AcqRel);
    }
}

/// Loom explores every interleaving of the modelled threads; the preemption
/// bound keeps the state space of the three-thread models in the seconds
/// range. Every case here is small enough that the bound is not the limiting
/// factor — the primitives' critical sections are a handful of atomics — but
/// it is stated explicitly rather than left to loom's default.
pub fn model(f: impl Fn() + Sync + Send + 'static) {
    let mut builder = loom::model::Builder::new();
    builder.preemption_bound = Some(3);
    builder.check(f);
}
