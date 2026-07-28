//! The hardware boundary: everything the scheduler core needs from the
//! world, in three traits stacked by what they have to know about.
//! [`Kicker`] knows only a CPU id; [`Machine`] adds the rest of the
//! task-blind surface (clock, one-shot timer, interrupt gate, halt, trace);
//! [`Hw`] adds the two operations that carry a task — the context switch and
//! the finalize sink. The kernel implements the first two with LAPIC
//! one-shot, targeted x2APIC ICR and TSC at migration stage 6, and the third
//! with the asm switch at stage 7; the simulator implements all three over a
//! virtual clock and vcpu bookkeeping (stage 4). No scheduling decision,
//! state transition or ordering-sensitive code may live behind them.

use crate::cpu::{RunToken, SleepToken};
use crate::task::{SchedPayload, TaskAccounting, TaskKey};

/// CPU identity. Always a field or a parameter, never an ambient query —
/// `Hw` deliberately has no `cpu_id()`, so a wrong-CPU lookup is
/// unrepresentable in the core (spec §10.1).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct CpuId(pub u32);

/// Absolute nanoseconds: since boot in the kernel, virtual-clock time in the
/// simulator.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Nanos(pub u64);

impl Nanos {
    pub const ZERO: Nanos = Nanos(0);

    pub fn after(self, ns: u64) -> Nanos {
        Nanos(self.0.saturating_add(ns))
    }

    /// Elapsed since `earlier`. Saturating rather than wrapping: a pass that
    /// samples a clock older than the last one is a driver bug, and charging
    /// a colossal interval would hide it behind absurd accounting instead of
    /// leaving the counters honest.
    pub fn since(self, earlier: Nanos) -> u64 {
        self.0.saturating_sub(earlier.0)
    }
}

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

/// The one effect a wake path needs from the world: the targeted kick IPI a
/// [`crate::mailbox::Kick::Send`] obliges the poster to deliver. Split out of
/// [`Machine`] so wait queues and the retire protocol — which run at any wake
/// site, not inside a scheduler pass — depend on nothing else.
pub trait Kicker: Sync {
    /// Targeted kick IPI. Never broadcast.
    fn kick(&self, target: CpuId);
}

/// The half of the hardware surface that says nothing about tasks: clock,
/// one-shot timer, interrupt gate, halt, resched request, trace sink.
///
/// Separate from [`Hw`] because it is separately *implementable*. Migration
/// stage 6 puts the kernel's LAPIC/TSC/ICR behind this trait while the old
/// scheduler still runs, so the surface meets real interrupts a stage before
/// anything depends on it — and a stage before the kernel has a
/// [`SchedPayload`] to name. A single trait would have made that impossible:
/// `switch` and `release` are the only members that need the payload, and
/// they are exactly the two the cutover brings.
pub trait Machine: Kicker + 'static {
    type IrqGuard;

    /// Sampled ONCE per pass by the driver and threaded as a value — the
    /// core never reads the clock mid-flight.
    fn now(&self) -> Nanos;

    /// Program the one-shot timer for an **absolute** deadline. The kernel's
    /// LAPIC one-shot is relative, so it converts; TSC-deadline mode is
    /// absolute and will not.
    fn set_timer(&self, deadline: Nanos);

    fn stop_timer(&self);

    /// Kernel: cli/sti RAII. Sim: gates event delivery for this vcpu.
    ///
    /// Note for whoever reaches for this: it does **not** fit the one site the
    /// spec names for it (§7.5's "cli / final recheck / sti;hlt"). Measured at
    /// stage 6 against the kernel: both exits from that recheck must *set* IF
    /// unconditionally — the halt exit because `sti;hlt` is one atom, and the
    /// stay-awake exit because the kernel's panic recovery enters the idle
    /// loop with IF already 0, so restoring the caller's flags would strand
    /// that CPU. An RAII guard can express neither. The kernel therefore
    /// implements this member and calls it nowhere; the core does not call it
    /// either. It is a designed surface with no user in either world.
    fn irq_guard(&self) -> Self::IrqGuard;

    /// Enable interrupts and halt, atomically — on x86 the `sti;hlt` pair and
    /// its STI shadow, which is why this is one operation and not an
    /// [`Self::irq_guard`] drop followed by a halt. A wake that lands in
    /// between would be consumed as an ordinary interrupt and then slept
    /// through. Returns once an interrupt has been taken.
    ///
    /// The *decision* to halt is not here: the final recheck reads scheduler
    /// state, so it lives above the boundary and its proof is [`SleepToken`].
    fn halt(&self);

    /// Ask `cpu` to take its next safe point. The core needs this for exactly
    /// one case the spec spells out (§7.6): a `Retire` whose target is the
    /// task currently *running* cannot be yanked mid-syscall, so it is asked
    /// to die at its next safe point instead. Not in the spec's trait list —
    /// which leaves that request without a way to reach the driver.
    fn need_resched(&self, cpu: CpuId);

    fn trace(&self, ev: TraceEvent);

    /// Halt on the strength of a [`SleepToken`]. Provided, and deliberately
    /// not overridable in spirit: the token is proof, [`Self::halt`] is the
    /// effect, and keeping the two apart is what lets a driver that cannot
    /// yet mint a token (stage 6) still exercise the halt.
    fn idle_wait(&self, token: SleepToken) {
        let _consumed = token;
        self.halt();
    }
}

/// The complete hardware surface. Everything above this trait — task types,
/// state word, transitions, run queue, fairness math, mailbox, doorbell,
/// sleep handshake, ticket protocol, deadline heap, pass logic — is real and
/// shared between the kernel and the simulator; everything behind it is
/// LAPIC/TSC/ICR/asm in the kernel and virtual time/pending-IPI bookkeeping
/// in the sim (spec §10.1).
pub trait Hw: Machine {
    type Payload: SchedPayload;

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

    /// Finalize sink: the environment reclaims a dead task's payload and its
    /// accounting, both handed over exactly once (spec §9.3).
    fn release(&self, key: TaskKey, payload: Self::Payload, acct: TaskAccounting);
}
