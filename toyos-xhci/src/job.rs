//! The one operation this controller has submitted and not seen the end of.
//!
//! A driver that may not wait has to be able to say what it is waiting *for*,
//! and to say it in a form a later pass can compare an arriving event against.
//! That is all this is: what ends the wait, when the wait stops being worth
//! having, and the answer once one arrives.

use crate::port::Nanos;

/// What the controller has to produce for an outstanding operation to be over.
///
/// **Both arms carry the physical address of the TRB whose completion this
/// is**, which is what the event names in its first two dwords: a Command
/// Completion Event names its Command TRB (xHCI 1.2 §6.4.2.2) and a Transfer
/// Event names the Transfer TRB that generated it (§6.4.2.1, with ED clear —
/// no TRB this driver enqueues sets Event Data). Matching on anything coarser
/// — "the next Command Completion Event", or "the next completion on this
/// endpoint" — hands an operation that ran out its deadline and answered
/// afterwards to whatever asked next.
///
/// **The transfer arm did the coarser thing until 2026-08-20**, with
/// [`Stages::DataThenStatus`] standing in for the ambiguity a control
/// transfer's two completions on one endpoint create. The two are now one
/// answer: a stage is named by its own TRB, so the second stage is not
/// something the *count* has to keep track of, and an abandoned transfer's late
/// completion matches nothing rather than matching the next asker.
///
/// `slot` and `dci` stay beside the address because they cost nothing and are a
/// second, independent statement of the same fact: an event whose endpoint
/// disagrees with the TRB the driver put on that endpoint's ring is a
/// controller contradicting itself, and one this driver must not act on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Await {
    Command { trb: u64 },
    Transfer { slot: u8, dci: u8, trb: u64 },
}

/// xHCI 1.2 Table 6-90's Success, and its Short Packet — a transfer that ended
/// on a packet smaller than the endpoint's maximum, which is a success with a
/// residue and not an error. Together they are the codes after which the
/// controller still owes whatever stages are left.
pub const CC_SUCCESS: u32 = 1;
pub const CC_SHORT_PACKET: u32 = 13;

/// What the controller still owes one operation after the completion it was
/// submitted on.
///
/// **A control transfer with a data stage produces two completions.** The data
/// stage carries IOC so the driver can learn how many bytes actually arrived,
/// and the status stage carries its own — so an operation that ended on the
/// first would leave the second to be matched by whatever asked next on the
/// same endpoint.
///
/// **A second [`Await`] and not a count**, which is the difference the TRB
/// address makes: the two stages are two TRBs at two addresses, so what is owed
/// after the data stage is a *named* event rather than "one more of something
/// on this endpoint". A count could only be spent by whatever arrived next.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stages {
    /// Nothing further: the operation is over on the completion of the TRB it
    /// was submitted with.
    One,
    /// A control transfer's status stage, on its own TRB, after the data stage
    /// the operation was submitted with.
    DataThenStatus(Await),
}

/// How an operation ended.
///
/// **Two answered variants and not one number named by context.** A Command
/// Completion Event and a Transfer Event carry a second value each and they are
/// different values in different fields — the Slot ID the controller allocated,
/// and the bytes of the buffer it did not move. One field for both would be a
/// number whose meaning only a comment states, and reading a command's residue
/// would compile.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// A command completed. `slot` is the event's Slot ID field, which is the
    /// controller's answer to Enable Slot and an echo of what was asked for
    /// every other command.
    Command { code: u32, slot: u8 },
    /// A transfer completed. `code` is the last stage's — a failing data stage
    /// ends the operation where it is — and `residue` is what the *first* stage
    /// said it did not move, because that is the only stage carrying a buffer.
    Transfer { code: u32, residue: u32 },
    /// Nothing arrived inside the deadline. Distinct from a failing code
    /// because the two say different things about the device: a code is a
    /// device that spoke, and this is one that did not.
    Silent,
}

impl Outcome {
    /// Whether the controller answered Success, which is what most callers ask
    /// and the only thing they do with the code.
    pub fn succeeded(self) -> bool {
        matches!(
            self,
            Self::Command { code: CC_SUCCESS, .. } | Self::Transfer { code: CC_SUCCESS, .. }
        )
    }

    /// The completion code, and `None` for an operation nothing answered.
    pub fn code(self) -> Option<u32> {
        match self {
            Self::Command { code, .. } | Self::Transfer { code, .. } => Some(code),
            Self::Silent => None,
        }
    }
}

struct Job<W> {
    what: W,
    /// What the **next** completion this operation owes must name. A two-stage
    /// control transfer moves it on to its status stage when the data stage
    /// arrives, so the slot never owes an unnamed event.
    on: Await,
    /// The stage after [`Self::on`], and `None` once nothing further is owed.
    then: Option<Await>,
    /// Wall clock, and deliberately not a spin count: the pass that submitted
    /// this gave itself back, so what notices the deadline is a later pass and
    /// not this one.
    deadline: Nanos,
    answer: Option<Outcome>,
    /// The first completion's second number, and `None` until one has arrived.
    /// Kept apart from `answer` because for a two-stage transfer the two come
    /// from different stages: only the data stage carries a buffer.
    param: Option<u32>,
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
    pub fn submit(&mut self, what: W, on: Await, stages: Stages, deadline: Nanos) {
        assert!(self.job.is_none(), "a second operation was submitted over an outstanding one");
        let then = match stages {
            Stages::One => None,
            Stages::DataThenStatus(status) => Some(status),
        };
        self.job = Some(Job { what, on, then, deadline, answer: None, param: None });
    }

    /// Offer an arriving completion to the outstanding operation, and say
    /// whether it was the one being waited for.
    ///
    /// **Recorded, never acted on here.** The drain that produces these runs
    /// inside the waits of a caller that is after one particular event, so
    /// anything done from this point would consume events that caller owns.
    ///
    /// `true` also for a stage that is not the last: the completion belongs to
    /// this operation whether or not it ends it, and a caller that took it back
    /// would hand the data stage of its own transfer to a bound device.
    ///
    /// `param` is the event's second number — the Slot ID for a command and the
    /// residue for a transfer — and which it is comes from `by` rather than
    /// from the caller saying, so the two cannot be swapped at a call site.
    pub fn answered(&mut self, by: Await, code: u32, param: u32) -> bool {
        let Some(job) = self.job.as_mut() else { return false };
        if job.on != by || job.answer.is_some() {
            return false;
        }
        let param = *job.param.get_or_insert(param);
        // A stage that did not succeed is the end of the operation whatever is
        // still owed: an errored data stage halts the endpoint, so the status
        // TRB behind it never runs and waiting for it spends the whole deadline
        // learning that. The `take` is what makes that true of the *slot* and
        // not only of this call — a failed data stage leaves nothing owed, so
        // the status TRB's own event, should the controller produce one anyway,
        // matches nothing.
        match job.then.take() {
            Some(status) if matches!(code, CC_SUCCESS | CC_SHORT_PACKET) => job.on = status,
            _ => {
                job.answer = Some(match by {
                    Await::Command { .. } => Outcome::Command { code, slot: param as u8 },
                    Await::Transfer { .. } => Outcome::Transfer { code, residue: param },
                })
            }
        }
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
            Some(outcome) => outcome,
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
    /// A control transfer's two stages on one endpoint: two TRBs, sixteen bytes
    /// apart, because that is how [`crate::job`]'s caller enqueues them.
    const EP0_DATA: Await = Await::Transfer { slot: 3, dci: 1, trb: 0x2000 };
    const EP0_STATUS: Await = Await::Transfer { slot: 3, dci: 1, trb: 0x2010 };
    /// A bulk transfer, which is one TRB and one completion.
    const BULK: Await = Await::Transfer { slot: 3, dci: 4, trb: 0x3000 };

    fn answered(code: u32) -> Outcome {
        Outcome::Command { code, slot: 0 }
    }

    fn slot() -> Outstanding<&'static str> {
        let mut o = Outstanding::EMPTY;
        o.submit("teardown", CMD, Stages::One, 100);
        o
    }

    /// A control transfer with a data stage, which is what every descriptor read
    /// in the driver is.
    fn two_stage() -> Outstanding<&'static str> {
        let mut o = Outstanding::EMPTY;
        o.submit("descriptor", EP0_DATA, Stages::DataThenStatus(EP0_STATUS), 100);
        o
    }

    #[test]
    fn an_empty_slot_is_not_busy_and_has_no_deadline() {
        let mut o: Outstanding<&str> = Outstanding::EMPTY;
        assert!(!o.busy());
        assert_eq!(o.wake_at(), None);
        assert!(o.finished(u64::MAX).is_none());
        assert!(!o.answered(CMD, 1, 0));
    }

    #[test]
    fn a_completion_for_another_trb_is_not_this_one() {
        let mut o = slot();
        assert!(!o.answered(Await::Command { trb: 0x2000 }, 1, 0));
        assert!(!o.answered(BULK, 1, 0));
        assert!(o.finished(99).is_none());
        assert!(o.answered(CMD, 1, 0));
    }

    #[test]
    fn a_transfer_matches_on_its_trb_and_on_both_halves_of_its_endpoint() {
        let mut o = Outstanding::EMPTY;
        o.submit((), BULK, Stages::One, 100);
        assert!(!o.answered(Await::Transfer { slot: 3, dci: 5, trb: 0x3000 }, 1, 0));
        assert!(!o.answered(Await::Transfer { slot: 4, dci: 4, trb: 0x3000 }, 1, 0));
        assert!(!o.answered(Await::Transfer { slot: 3, dci: 4, trb: 0x3010 }, 1, 0));
        assert!(o.answered(BULK, 1, 0));
    }

    /// **The defect the TRB address closes.** A transfer the driver stopped
    /// waiting for is still the device's to answer, and the answer arrives on
    /// the same endpoint as whatever asked next. Matching on the endpoint hands
    /// the first transfer's completion — and its residue — to the second.
    #[test]
    fn a_late_completion_from_an_abandoned_transfer_answers_nobody() {
        let mut o = Outstanding::EMPTY;
        o.submit("abandoned", BULK, Stages::One, 100);
        assert_eq!(o.cancel(), Some("abandoned"));
        let next = Await::Transfer { slot: 3, dci: 4, trb: 0x3010 };
        o.submit("the next command", next, Stages::One, 200);
        // The abandoned transfer's completion, on the same slot and the same
        // endpoint, carrying a residue that is not this transfer's.
        assert!(!o.answered(BULK, CC_SUCCESS, 4096));
        assert!(o.finished(150).is_none());
        assert!(o.answered(next, CC_SUCCESS, 0));
        assert_eq!(
            o.finished(150),
            Some(("the next command", Outcome::Transfer { code: CC_SUCCESS, residue: 0 }))
        );
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
        assert!(o.answered(CMD, 6, 0));
        assert_eq!(o.finished(u64::MAX), Some(("teardown", answered(6))));
    }

    #[test]
    fn only_the_first_answer_counts() {
        let mut o = slot();
        assert!(o.answered(CMD, 1, 0));
        assert!(!o.answered(CMD, 6, 0));
        assert_eq!(o.finished(0), Some(("teardown", answered(1))));
    }

    #[test]
    fn a_cancelled_operation_leaves_the_slot_free() {
        let mut o = slot();
        assert_eq!(o.what(), Some(&"teardown"));
        assert_eq!(o.cancel(), Some("teardown"));
        assert!(!o.busy());
        assert!(!o.answered(CMD, 1, 0));
    }

    #[test]
    #[should_panic(expected = "outstanding")]
    fn submitting_over_an_outstanding_operation_panics() {
        let mut o = slot();
        o.submit("recovery", Await::Command { trb: 0x2000 }, Stages::One, 200);
    }

    /// The defect the second stage exists for: both completions land in one
    /// drain, and an operation that ended on the data stage would leave the
    /// status stage to answer whatever asked next on the same endpoint.
    #[test]
    fn a_data_stage_does_not_end_a_transfer_that_still_owes_its_status() {
        let mut o = two_stage();
        assert!(o.answered(EP0_DATA, CC_SUCCESS, 4));
        assert!(o.finished(0).is_none());
        assert!(o.busy());
        assert!(o.answered(EP0_STATUS, CC_SUCCESS, 0));
        assert_eq!(
            o.finished(0),
            Some(("descriptor", Outcome::Transfer { code: CC_SUCCESS, residue: 4 }))
        );
    }

    /// The stages are named and therefore ordered: a status stage cannot answer
    /// the data stage the operation is still waiting for, and a data stage
    /// cannot be counted twice.
    #[test]
    fn a_stage_is_answered_by_its_own_trb_and_in_its_own_order() {
        let mut o = two_stage();
        assert!(!o.answered(EP0_STATUS, CC_SUCCESS, 0));
        assert!(o.answered(EP0_DATA, CC_SUCCESS, 4));
        assert!(!o.answered(EP0_DATA, CC_SUCCESS, 4));
        assert!(o.answered(EP0_STATUS, CC_SUCCESS, 0));
    }

    /// Only the data stage carries a buffer, so its residue is the one that
    /// says how many bytes arrived — and the status stage's is meaningless.
    #[test]
    fn the_residue_is_the_first_stages_and_the_code_is_the_last() {
        let mut o = two_stage();
        assert!(o.answered(EP0_DATA, CC_SHORT_PACKET, 10));
        assert!(o.answered(EP0_STATUS, CC_SUCCESS, 999));
        assert_eq!(
            o.finished(0),
            Some(("descriptor", Outcome::Transfer { code: CC_SUCCESS, residue: 10 }))
        );
    }

    /// A failing data stage halts the endpoint, so the status TRB behind it
    /// never runs. Waiting for it spends the whole deadline learning that —
    /// and a status event the controller produces anyway belongs to nobody.
    #[test]
    fn a_failing_stage_ends_the_transfer_whatever_is_still_owed() {
        let mut o = two_stage();
        assert!(o.answered(EP0_DATA, 6, 8));
        assert!(!o.answered(EP0_STATUS, CC_SUCCESS, 0));
        assert_eq!(
            o.finished(0),
            Some(("descriptor", Outcome::Transfer { code: 6, residue: 8 }))
        );
    }

    /// A status stage that fails after a data stage that did not is the
    /// device's verdict, and it is the code the caller sees.
    #[test]
    fn a_failing_status_stage_is_the_answer() {
        let mut o = two_stage();
        assert!(o.answered(EP0_DATA, CC_SUCCESS, 0));
        assert!(o.answered(EP0_STATUS, 4, 0));
        assert_eq!(
            o.finished(0),
            Some(("descriptor", Outcome::Transfer { code: 4, residue: 0 }))
        );
    }

    #[test]
    fn a_transfer_that_answers_one_stage_and_stops_is_silent() {
        let mut o = two_stage();
        assert!(o.answered(EP0_DATA, CC_SUCCESS, 0));
        assert_eq!(o.finished(100), Some(("descriptor", Outcome::Silent)));
    }
}
