//! The one operation this controller has submitted and not seen the end of.
//!
//! A driver that may not wait has to be able to say what it is waiting *for*,
//! and to say it in a form a later pass can compare an arriving event against.
//! That is all this is: what ends the wait, when the wait stops being worth
//! having, and the answer once one arrives.
//!
//! `specs/xhci-port-machine-plan.md` X2 is the design.

use crate::port::Nanos;

/// What the controller has to produce for an outstanding operation to be over.
///
/// The command arm carries the **physical address of the Command TRB**, which
/// is what a Command Completion Event names in its first two dwords (xHCI 1.2
/// §6.4.2.2). Matching on anything coarser — "the next Command Completion
/// Event" — hands a command that ran out its deadline and answered afterwards
/// to whatever asked next.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Await {
    Command { trb: u64 },
    Transfer { slot: u8, dci: u8 },
}

/// How an operation ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// The controller answered with this completion code.
    Answered(u32),
    /// Nothing arrived inside the deadline. Distinct from a failing code
    /// because the two say different things about the device: a code is a
    /// device that spoke, and this is one that did not.
    Silent,
}

struct Job<W> {
    what: W,
    on: Await,
    /// Wall clock, and deliberately not a spin count: the pass that submitted
    /// this gave itself back, so what notices the deadline is a later pass and
    /// not this one.
    deadline: Nanos,
    answer: Option<u32>,
}

/// The slot, holding at most one operation.
///
/// **One and not a queue.** The command ring is a single queue the controller
/// works in order, the driver this replaces ran every command strictly serially
/// because each was a submit followed by its own wait, and a second slot would
/// buy concurrency the hardware does not offer at the price of the cancellation
/// rule below having a simple form. What a caller sees when the slot is taken
/// is [`Self::busy`], and every caller of that defers rather than queues.
pub struct Outstanding<W> {
    job: Option<Job<W>>,
}

impl<W> Default for Outstanding<W> {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl<W> Outstanding<W> {
    pub const EMPTY: Self = Self { job: None };

    /// Whether an operation is in flight. A caller that would submit one, free
    /// memory an in-flight one still names, or start work that needs the
    /// controller's answer first, defers on this.
    pub fn busy(&self) -> bool {
        self.job.is_some()
    }

    /// What the outstanding operation is for, for a caller deciding whether it
    /// is the one to cancel.
    pub fn what(&self) -> Option<&W> {
        self.job.as_ref().map(|j| &j.what)
    }

    /// Record an operation the caller has just submitted to the controller.
    ///
    /// Panics if one is already outstanding: the slot is the whole reason
    /// [`Self::busy`] exists, and a caller that submitted without asking has
    /// left a completion nothing will ever match.
    pub fn submit(&mut self, what: W, on: Await, deadline: Nanos) {
        assert!(self.job.is_none(), "a second operation was submitted over an outstanding one");
        self.job = Some(Job { what, on, deadline, answer: None });
    }

    /// Offer an arriving completion to the outstanding operation, and say
    /// whether it was the one being waited for.
    ///
    /// **Recorded, never acted on here.** The drain that produces these runs
    /// inside the waits of a caller that is after one particular event, so
    /// anything done from this point would consume events that caller owns.
    pub fn answered(&mut self, by: Await, code: u32) -> bool {
        let Some(job) = self.job.as_mut() else { return false };
        if job.on != by || job.answer.is_some() {
            return false;
        }
        job.answer = Some(code);
        true
    }

    /// Take the operation if it is over, with what it was for.
    ///
    /// An answer wins over the clock: a completion that arrived inside the
    /// deadline is the controller's answer whenever the caller gets round to
    /// asking, and calling it silent because a pass was late would abandon a
    /// device that did everything right.
    pub fn finished(&mut self, now: Nanos) -> Option<(W, Outcome)> {
        let job = self.job.as_ref()?;
        let outcome = match job.answer {
            Some(code) => Outcome::Answered(code),
            None if now >= job.deadline => Outcome::Silent,
            None => return None,
        };
        Some((self.job.take()?.what, outcome))
    }

    /// Forget the outstanding operation, for a caller that has learnt its
    /// answer cannot matter — the device it was for has left the bus. The
    /// completion may still arrive and is then an event addressed to nobody,
    /// which is what every abandoned wait already produced.
    pub fn cancel(&mut self) -> Option<W> {
        Some(self.job.take()?.what)
    }

    /// When the deadline runs out, so a caller with nothing else to do knows
    /// when to come back. `None` when nothing is outstanding.
    pub fn wake_at(&self) -> Option<Nanos> {
        Some(self.job.as_ref()?.deadline)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CMD: Await = Await::Command { trb: 0x1000 };

    fn slot() -> Outstanding<&'static str> {
        let mut o = Outstanding::EMPTY;
        o.submit("teardown", CMD, 100);
        o
    }

    #[test]
    fn an_empty_slot_is_not_busy_and_has_no_deadline() {
        let mut o: Outstanding<&str> = Outstanding::EMPTY;
        assert!(!o.busy());
        assert_eq!(o.wake_at(), None);
        assert!(o.finished(u64::MAX).is_none());
        assert!(!o.answered(CMD, 1));
    }

    #[test]
    fn a_completion_for_another_trb_is_not_this_one() {
        let mut o = slot();
        assert!(!o.answered(Await::Command { trb: 0x2000 }, 1));
        assert!(!o.answered(Await::Transfer { slot: 1, dci: 1 }, 1));
        assert!(o.finished(99).is_none());
        assert!(o.answered(CMD, 1));
    }

    #[test]
    fn a_transfer_matches_on_both_halves_of_its_endpoint() {
        let mut o = Outstanding::EMPTY;
        o.submit((), Await::Transfer { slot: 3, dci: 1 }, 100);
        assert!(!o.answered(Await::Transfer { slot: 3, dci: 2 }, 1));
        assert!(!o.answered(Await::Transfer { slot: 4, dci: 1 }, 1));
        assert!(o.answered(Await::Transfer { slot: 3, dci: 1 }, 1));
    }

    #[test]
    fn the_deadline_produces_silence_and_nothing_else() {
        let mut o = slot();
        assert!(o.finished(99).is_none());
        assert_eq!(o.finished(100), Some(("teardown", Outcome::Silent)));
        assert!(!o.busy());
    }

    /// The case a pass-based driver hits routinely: the completion landed on
    /// time and the pass that looks is late. Calling that silent would abandon
    /// a device that answered.
    #[test]
    fn an_answer_outlives_the_deadline() {
        let mut o = slot();
        assert!(o.answered(CMD, 6));
        assert_eq!(o.finished(u64::MAX), Some(("teardown", Outcome::Answered(6))));
    }

    #[test]
    fn only_the_first_answer_counts() {
        let mut o = slot();
        assert!(o.answered(CMD, 1));
        assert!(!o.answered(CMD, 6));
        assert_eq!(o.finished(0), Some(("teardown", Outcome::Answered(1))));
    }

    #[test]
    fn a_cancelled_operation_leaves_the_slot_free() {
        let mut o = slot();
        assert_eq!(o.what(), Some(&"teardown"));
        assert_eq!(o.cancel(), Some("teardown"));
        assert!(!o.busy());
        assert!(!o.answered(CMD, 1));
    }

    #[test]
    #[should_panic(expected = "outstanding")]
    fn submitting_over_an_outstanding_operation_panics() {
        let mut o = slot();
        o.submit("recovery", Await::Command { trb: 0x2000 }, 200);
    }
}
