//! What the simulator attaches to a task, and the environment-supplied bits
//! the core crate refuses to implement itself (spec §10.1).
//!
//! The payload carries a **real** `Arc` to a mock address space. That is the
//! crash.md detector: the kernel's bug was an address-space `Arc` dropped
//! twice because a task existed in two places at once, and here the refcount
//! is checked against the set of live tasks after every single step
//! (invariant I8).

use std::sync::{Arc, Mutex};

use toyos_sched::fair::ShareState;
use toyos_sched::mailbox::PreemptGuard;
use toyos_sched::sync::LeafLock;
use toyos_sched::task::{SchedPayload, TaskKey};
use toyos_sched::waitq::WaitList;

use crate::msg::SimMsg;

/// The environment's small shared cell. A `Mutex` in a single-threaded
/// step machine is uncontended by construction; what matters is that the core
/// gets its interior mutability from outside, so it needs no `unsafe` of its
/// own.
pub struct StdLock<T>(Mutex<T>);

impl<T> StdLock<T> {
    pub fn new(value: T) -> Self {
        Self(Mutex::new(value))
    }
}

impl<T: Send> LeafLock<T> for StdLock<T> {
    fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        f(&mut self.0.lock().expect("the simulator never poisons a lock"))
    }
}

pub type SimWaitList = StdLock<WaitList<SimMsg>>;
pub type SimShareLock = StdLock<ShareState>;

/// The simulator explores interleavings by *choosing steps*, not by running
/// threads (spec §13's rejected host-thread-per-vCPU design). A step is
/// atomic, so the executing context provably cannot be descheduled inside
/// one.
pub struct SimPreempt;

// SAFETY: see above — a step is atomic in this world, which is a strictly
// stronger property than the preempt-disabled region the guard stands for.
#[allow(unsafe_code)]
unsafe impl PreemptGuard for SimPreempt {}

/// Stands in for the kernel's `AddressSpace`: one per process, referenced by
/// every task of that process and by the process record itself.
#[derive(Debug)]
pub struct MockAddressSpace {
    pub process: u32,
}

/// The per-task environment payload. Its `Drop` is the point of the whole
/// exercise: the `Arc` inside is released exactly once, by the one
/// `finalize()` that consumes the linear task value.
pub struct SimPayload {
    pub key: TaskKey,
    pub process: u32,
    pub address_space: Arc<MockAddressSpace>,
}

/// The saved context. Its contents are irrelevant to the model — what matters
/// is *whether* it was saved before the task moved, which the VM shadows by
/// key (invariant I11).
#[derive(Default)]
pub struct SimCtx {
    pub key: Option<TaskKey>,
}

impl SchedPayload for SimPayload {
    type Ctx = SimCtx;
    type ShareLock = SimShareLock;
}
