//! The timer plan — spec §8.4.
//!
//! A deadline lives in exactly one place: the `ParkedEntry` of the task that
//! owns it, on that task's home CPU (spec §6.1, §8.3). Only *ready* tasks
//! migrate, so a deadline can never end up on a CPU that no longer owns its
//! task, and a task that is not parked structurally cannot have one.
//!
//! There is no separate index, and the absence is the design: an ordered index
//! would hold the deadline a second time, and the pass that ended a park would
//! have to remember to retire both. It did not — `fire_deadlines` discarded the
//! index entry of a claim it lost and left the entry's copy standing, which is
//! a CPU reporting a deadline nothing will fire.

use crate::hw::Nanos;

/// What the one-shot timer must be programmed to at the end of a pass.
/// Produced by `finish()` after every parking change, applied *last* — which
/// is the whole proof of invariant T: there is no window between the last
/// change and the arming (spec §8.4).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[must_use = "a timer plan that is not applied is invariant T violated"]
pub enum TimerPlan {
    Arm(Nanos),
    Stop,
}

impl TimerPlan {
    /// `quantum_end` is present only while a task is running; `deadline` is
    /// the earliest a parked task on this CPU owes.
    pub fn compute(quantum_end: Option<Nanos>, deadline: Option<Nanos>) -> Self {
        match (quantum_end, deadline) {
            (Some(q), Some(d)) => TimerPlan::Arm(q.min(d)),
            (Some(q), None) => TimerPlan::Arm(q),
            (None, Some(d)) => TimerPlan::Arm(d),
            (None, None) => TimerPlan::Stop,
        }
    }

    /// Pull the arming forward to `at`, if a caller has a reason to be woken
    /// that no parked deadline and no quantum accounts for — today
    /// [`crate::cpu::Balance::PullWithRearm`]'s re-probe, and nothing else.
    ///
    /// One direction only, and that is what keeps invariant T out of it: the
    /// invariant bounds the armed instant from *above* by the earliest event the
    /// CPU owes, so a plan that only ever moves earlier cannot violate it. A
    /// method that could move the arming later would be a missed deadline
    /// waiting for a caller.
    pub fn no_later_than(self, at: Option<Nanos>) -> Self {
        match (self, at) {
            (_, None) => self,
            (TimerPlan::Stop, Some(at)) => TimerPlan::Arm(at),
            (TimerPlan::Arm(planned), Some(at)) => TimerPlan::Arm(planned.min(at)),
        }
    }

    pub fn armed(&self) -> Option<Nanos> {
        match self {
            TimerPlan::Arm(at) => Some(*at),
            TimerPlan::Stop => None,
        }
    }
}

/// Proof that a [`TimerPlan`] reached the hardware. [`crate::cpu::SleepToken`]
/// cannot be built without one, so "halted with a deadline pending and the
/// timer unarmed" is unrepresentable rather than asserted.
pub struct TimerApplied {
    armed: Option<Nanos>,
}

impl TimerApplied {
    pub(crate) fn new(armed: Option<Nanos>) -> Self {
        Self { armed }
    }

    pub fn armed(&self) -> Option<Nanos> {
        self.armed
    }
}
