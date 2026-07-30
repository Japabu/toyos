//! The per-CPU deadline heap and the timer plan — spec §8.3, §8.4.
//!
//! Every deadline lives in exactly one place: the home CPU's heap, maintained
//! by the same pass that owns the parking. Only *ready* tasks migrate, so a
//! deadline can never end up on a CPU that no longer owns its task (spec
//! §6.1) — the "blocked task's deadline on a migrated task" failure has no
//! representation.
//!
//! Deletion is lazy. A woken task's entry stays in the heap and is discarded
//! when it surfaces, validated against `parked`; paying O(log n) on every wake
//! to keep the heap exact would buy nothing, since the validation is needed
//! anyway (a wake and a timeout can be in flight at once).

use alloc::collections::BinaryHeap;
use core::cmp::Reverse;

use crate::hw::Nanos;
use crate::task::TaskKey;

/// Is this heap entry still the truth? The pass answers from `parked`: a key
/// that is gone, or whose `ParkedEntry.deadline` no longer matches, is stale.
pub trait DeadlineOracle {
    fn is_current(&self, key: TaskKey, deadline: Nanos) -> bool;
}

pub struct DeadlineHeap {
    entries: BinaryHeap<Reverse<(Nanos, TaskKey)>>,
}

impl DeadlineHeap {
    pub fn new() -> Self {
        Self {
            entries: BinaryHeap::new(),
        }
    }

    pub fn insert(&mut self, deadline: Nanos, key: TaskKey) {
        self.entries.push(Reverse((deadline, key)));
    }

    /// The next due entry, stale ones discarded on the way. `None` means
    /// nothing is due at `now`.
    pub fn pop_due(&mut self, now: Nanos, oracle: &impl DeadlineOracle) -> Option<TaskKey> {
        loop {
            let &Reverse((deadline, key)) = self.entries.peek()?;
            if deadline > now {
                return None;
            }
            self.entries.pop();
            if oracle.is_current(key, deadline) {
                return Some(key);
            }
        }
    }

    /// The earliest deadline that is still real, discarding stale entries
    /// above it. Taking `&mut self` is deliberate: the discard is the only
    /// way to answer honestly, and an honest answer is what invariant T
    /// (§8.4) is checked against.
    pub fn min_valid(&mut self, oracle: &impl DeadlineOracle) -> Option<Nanos> {
        loop {
            let &Reverse((deadline, key)) = self.entries.peek()?;
            if oracle.is_current(key, deadline) {
                return Some(deadline);
            }
            self.entries.pop();
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl Default for DeadlineHeap {
    fn default() -> Self {
        Self::new()
    }
}

/// What the one-shot timer must be programmed to at the end of a pass.
/// Produced by `finish()` after every heap mutation, applied *last* — which
/// is the whole proof of invariant T: there is no window between the last
/// mutation and the arming (spec §8.4).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[must_use = "a timer plan that is not applied is invariant T violated"]
pub enum TimerPlan {
    Arm(Nanos),
    Stop,
}

impl TimerPlan {
    /// `quantum_end` is present only while a task is running; `deadline` is
    /// the validated heap minimum.
    pub fn compute(quantum_end: Option<Nanos>, deadline: Option<Nanos>) -> Self {
        match (quantum_end, deadline) {
            (Some(q), Some(d)) => TimerPlan::Arm(q.min(d)),
            (Some(q), None) => TimerPlan::Arm(q),
            (None, Some(d)) => TimerPlan::Arm(d),
            (None, None) => TimerPlan::Stop,
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
