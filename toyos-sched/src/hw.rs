//! The hardware boundary: everything the scheduler core needs from the
//! world, in three traits stacked by what they have to know about.
//! [`Kicker`] knows only a CPU id; [`Machine`] adds the rest of the
//! task-blind surface (clock, one-shot timer, interrupt gate, halt, trace);
//! [`Hw`] adds the two operations that carry a task — the context switch and
//! the finalize sink. The kernel implements them with LAPIC one-shot,
//! targeted x2APIC ICR, TSC and the asm switch; the simulator over a virtual
//! clock and vcpu bookkeeping. No scheduling decision, state transition or
//! ordering-sensitive code may live behind them.

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
    /// samples a clock older than the last one is a driver bug, and wrapping
    /// would hide it behind a colossal charge.
    pub fn since(self, earlier: Nanos) -> u64 {
        self.0.saturating_sub(earlier.0)
    }
}

/// One scheduling-relevant event, in the format shared by the kernel's
/// per-CPU binary trace ring and the simulator's recorder.
/// `toyos-sched-sim replay --from-qemu` converts a captured kernel stream
/// into a sim event script (spec §10.4).
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
/// one-shot timer, interrupt gate, halt, resched request, trace sink. Split
/// from [`Hw`] so an implementor needs no [`SchedPayload`] to provide it.
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
    /// Has no caller in either world, and does **not** fit the site it looks
    /// like it should (§7.5's "cli / final recheck / sti;hlt"): both exits
    /// from that recheck must *set* IF unconditionally — the halt exit because
    /// `sti;hlt` is one atom, the stay-awake exit because panic recovery
    /// enters the idle loop with IF already 0 — and an RAII guard restores
    /// the caller's flags instead.
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

    /// Ask `cpu` to take its next safe point. Needed for one case: a `Retire`
    /// whose target is the *running* task cannot be yanked mid-syscall, so it
    /// is asked to die at its next safe point instead (spec §7.6).
    fn need_resched(&self, cpu: CpuId);

    fn trace(&self, ev: TraceEvent);

    /// Halt on the strength of a [`SleepToken`]. The token is the proof and
    /// [`Self::halt`] is the effect; they stay separate types.
    fn idle_wait(&self, token: SleepToken) {
        let _consumed = token;
        self.halt();
    }
}

/// The complete hardware surface. Everything above this trait is shared
/// between the kernel and the simulator; everything behind it is
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
