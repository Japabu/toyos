//! The hardware boundary: everything the scheduler core needs from the
//! world, as one trait. The kernel implements it with LAPIC one-shot,
//! targeted x2APIC ICR and the asm switch (migration Stage 6); the simulator
//! implements it over a virtual clock and vcpu bookkeeping (Stage 4). No
//! scheduling decision, state transition or ordering-sensitive code may live
//! behind this trait.

use crate::cpu::{RunToken, SleepToken};
use crate::task::{SchedPayload, TaskKey};

/// CPU identity. Always a field or a parameter, never an ambient query —
/// `Hw` deliberately has no `cpu_id()`, so a wrong-CPU lookup is
/// unrepresentable in the core (spec §10.1).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct CpuId(pub u32);

/// Absolute nanoseconds: since boot in the kernel, virtual-clock time in the
/// simulator.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Nanos(pub u64);

/// One scheduling-relevant event in the format shared by the kernel's
/// per-CPU binary trace ring (Stage 6) and the simulator's recorder
/// (Stage 4). `toyos-sched-sim replay --from-qemu` converts a captured
/// kernel stream into a sim event script, turning a real-world anomaly into
/// a deterministic host-side repro (spec §10.4).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TraceEvent {
    pub ts: Nanos,
    pub cpu: CpuId,
    pub kind: TraceKind,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TraceKind {
    /// `task` was picked and dispatched.
    Schedule { task: TaskKey },
    Wake { task: TaskKey },
    Block { task: TaskKey },
    /// Two-phase wait commit parked the task (spec §8.1).
    ParkCommit { task: TaskKey },
    Migrate { task: TaskKey, to: CpuId },
    Adopt { task: TaskKey },
    Retire { task: TaskKey },
    IdleEnter,
    IdleExit,
    Irq,
    TimerFire,
}

/// The complete hardware surface. Everything above this trait — task types,
/// state word, transitions, run queue, fairness math, mailbox, doorbell,
/// sleep handshake, ticket protocol, deadline heap, pass logic — is real and
/// shared between the kernel and the simulator; everything behind it is
/// LAPIC/TSC/ICR/asm in the kernel and virtual time/pending-IPI bookkeeping
/// in the sim (spec §10.1).
/// The one effect a wake path needs from the world: the targeted kick IPI a
/// [`crate::mailbox::Kick::Send`] obliges the poster to deliver. Split out of
/// [`Hw`] so wait queues and the retire protocol — which run at any wake
/// site, not inside a scheduler pass — depend on nothing else.
pub trait Kicker: Sync {
    /// Targeted kick IPI. Never broadcast.
    fn kick(&self, target: CpuId);
}

pub trait Hw: Kicker + 'static {
    type Payload: SchedPayload;
    type IrqGuard;

    /// Sampled ONCE per pass by the driver and threaded as a value — the
    /// core never reads the clock mid-flight.
    fn now(&self) -> Nanos;

    /// Program the one-shot timer for an absolute deadline.
    fn set_timer(&self, deadline: Nanos);

    fn stop_timer(&self);

    /// Kernel: cli/sti RAII. Sim: gates event delivery for this vcpu.
    fn irq_guard(&self) -> Self::IrqGuard;

    /// Perform the context switch the token describes. Nothing
    /// scheduler-related runs after this on the old context — the pass that
    /// produced the token has already ended (spec §6.2).
    ///
    /// # Safety
    /// The token's pointers are valid: they were constructed by safe code
    /// into stable Box-backed task records and the records outlive the
    /// switch by construction. The implementor must not retain them.
    #[allow(unsafe_code)] // declaration only — the core constructs tokens in safe code (spec §4)
    unsafe fn switch(&self, token: RunToken<Self::Payload>);

    /// Kernel: cli / final recheck / sti;hlt. Sim: mark the vcpu sleeping.
    fn idle_wait(&self, token: SleepToken);

    /// Finalize sink: the environment reclaims a dead task's payload.
    fn release(&self, key: TaskKey, payload: Self::Payload);

    fn trace(&self, ev: TraceEvent);
}
