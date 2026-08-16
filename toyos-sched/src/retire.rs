//! The retire protocol — spec §7.6: a sticky kill bit plus a message chase.
//!
//! **Termination argument, restated for `specs/completion-architecture-spec.md`
//! §7.2.** The kill bit is already set when the message is posted, so whichever
//! CPU ends up owning the task *schedules* it — into that CPU's dying list, or
//! by asking a running victim for a safe point — and it dies by its own `die`
//! at the first safe point its own unwind reaches. The chase is bounded by the
//! number of in-flight hops (≤1 in practice). Nothing scans; the home CPU in
//! the state word is the proof.
//!
//! The struck form said the owning CPU "converts it to a dead task on arrival",
//! and that conversion is exactly what §7.2 deleted: this kernel does not
//! unwind, so discarding the task value discarded every guard on its kernel
//! stack. The chase's bound is unchanged — what changed is what the last hop
//! does.

use crate::cpu::CpuHandles;
use crate::hw::{CpuId, Kicker};
use crate::mailbox::{Kick, PreemptGuard, SchedMsg, Urgency};
use crate::sync::Arc;
use crate::task::{TaskShared, TaskState};

/// Exactly one retirer exists per task (process teardown or thread kill), and
/// it holds this. Consumed by [`RetireTicket::post`].
#[must_use = "a claimed retire must be posted"]
pub struct RetireTicket<'a, M> {
    shared: &'a Arc<TaskShared<M>>,
}

/// Claim the right to retire `shared`: sets the sticky KILL and
/// RETIRE_QUEUED bits. Panics if a retire is already queued — a second
/// concurrent retire of one task is a kernel bug, not a condition to tolerate.
pub fn begin<M>(shared: &Arc<TaskShared<M>>) -> RetireTicket<'_, M> {
    assert!(
        shared.claim_retire(),
        "a second retirer for task {:?}: single-retirer is a kernel invariant",
        shared.key(),
    );
    RetireTicket { shared }
}

impl<M: SchedMsg> RetireTicket<'_, M> {
    /// Post `Msg::Retire` to wherever the task currently lives. `None` means
    /// the task is already dead — there is nobody left to ask for a safe
    /// point, and no stack left to unwind.
    pub fn post(
        self,
        cpus: &CpuHandles<M>,
        kicker: &impl Kicker,
        preempt: &impl PreemptGuard,
    ) -> Option<CpuId> {
        post_retire(self.shared, cpus, kicker, preempt)
    }
}

/// The chase step, run by a consumer that found the task somewhere else: the
/// state word now names another CPU (an `InTransit` adopt in flight, or a
/// migration that landed after the retirer read the word), so the *same*
/// retire node is re-posted there. Legal precisely because this consumer just
/// unlinked it (spec §7.6, §7.2 N1).
pub fn chase<M: SchedMsg>(
    shared: &Arc<TaskShared<M>>,
    cpus: &CpuHandles<M>,
    kicker: &impl Kicker,
    preempt: &impl PreemptGuard,
) -> Option<CpuId> {
    debug_assert!(
        shared.kill_pending(),
        "chasing a retire for a task that was never killed",
    );
    post_retire(shared, cpus, kicker, preempt)
}

fn post_retire<M: SchedMsg>(
    shared: &Arc<TaskShared<M>>,
    cpus: &CpuHandles<M>,
    kicker: &impl Kicker,
    preempt: &impl PreemptGuard,
) -> Option<CpuId> {
    let target = home_of(shared.state())?;
    // The sticky RETIRE_QUEUED bit admits one poster, and a chase only runs
    // after the consumer released the node, so it is free either way (I12).
    let slot = shared
        .retire_node()
        .claim()
        .expect("one retire in flight per task: node must be free");
    let handle = cpus.get(target);
    // Retire always preempts: a killed task must stop running promptly.
    if handle.post(slot, M::retire(shared.clone()), Urgency::Preempt, preempt) == Kick::Send {
        kicker.kick(target);
    }
    Some(target)
}

/// Which CPU must handle the task's death. For a task in transit that is the
/// destination — the adopting CPU sees the kill bit and dispatches it into its
/// own dying list on arrival.
fn home_of(state: TaskState) -> Option<CpuId> {
    match state {
        TaskState::Running(cpu)
        | TaskState::Ready(cpu)
        | TaskState::Committing(cpu, _)
        | TaskState::Blocked(cpu)
        | TaskState::WakeQueued(cpu)
        | TaskState::InTransit(cpu) => Some(cpu),
        TaskState::Dead => None,
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::cpu::{CpuHandle, CpuHandles};
    use crate::mailbox::{mailbox, MailboxConsumer, NoPreempt};
    use crate::task::{Claim, TaskKey, WakeCause, WakeReason};
    use alloc::vec;
    use alloc::vec::Vec;
    use std::sync::Mutex;

    #[derive(Debug, PartialEq, Eq)]
    enum Msg {
        Wake(TaskKey),
        Retire(TaskKey),
    }

    impl SchedMsg for Msg {
        fn wake(key: TaskKey, _cause: WakeCause) -> Self {
            Msg::Wake(key)
        }
        fn retire(shared: Arc<TaskShared<Self>>) -> Self {
            Msg::Retire(shared.key())
        }
    }

    #[derive(Default)]
    struct Kicks(Mutex<Vec<CpuId>>);
    impl Kicker for Kicks {
        fn kick(&self, target: CpuId) {
            self.0.lock().unwrap().push(target);
        }
    }

    /// A [`Kicker`] that samples the victim's kill bit at the instant the IPI
    /// is issued, which is the only place that ordering can be observed.
    struct SamplingKicks {
        task: Arc<TaskShared<Msg>>,
        seen: Mutex<Vec<bool>>,
    }

    impl Kicker for SamplingKicks {
        fn kick(&self, _target: CpuId) {
            self.seen.lock().unwrap().push(self.task.kill_pending());
        }
    }

    const C0: CpuId = CpuId(0);
    const C1: CpuId = CpuId(1);

    fn world() -> (CpuHandles<Msg>, Vec<MailboxConsumer<Msg>>) {
        let (tx0, rx0) = mailbox();
        let (tx1, rx1) = mailbox();
        (
            CpuHandles::new(vec![CpuHandle::new(C0, tx0), CpuHandle::new(C1, tx1)]),
            vec![rx0, rx1],
        )
    }

    fn task(key: u64, state: TaskState) -> Arc<TaskShared<Msg>> {
        Arc::new(TaskShared::new(TaskKey(key), state))
    }

    #[test]
    fn retire_goes_to_the_home_cpu_and_always_kicks() {
        let (cpus, mut rx) = world();
        let kicks = Kicks::default();
        let t = task(1, TaskState::Blocked(C1));

        assert_eq!(begin(&t).post(&cpus, &kicks, &NoPreempt), Some(C1));
        assert!(t.kill_pending() && t.retire_queued());
        assert_eq!(rx[1].pop(&NoPreempt), Some(Msg::Retire(TaskKey(1))));
        assert_eq!(rx[0].pop(&NoPreempt), None);
        assert_eq!(&*kicks.0.lock().unwrap(), &[C1], "retire preempts");
    }

    #[test]
    #[should_panic(expected = "single-retirer is a kernel invariant")]
    fn a_second_retirer_fails_fast() {
        let (cpus, mut rx) = world();
        let kicks = Kicks::default();
        let t = task(1, TaskState::Blocked(C0));
        begin(&t).post(&cpus, &kicks, &NoPreempt);
        // Consume it first: an undelivered message would trip the node's own
        // drop bomb during unwinding and mask the panic under test.
        assert_eq!(rx[0].pop(&NoPreempt), Some(Msg::Retire(TaskKey(1))));
        let _ = begin(&t);
    }

    #[test]
    fn the_chase_reuses_the_same_node() {
        let (cpus, mut rx) = world();
        let kicks = Kicks::default();
        let t = task(1, TaskState::Ready(C0));

        assert_eq!(begin(&t).post(&cpus, &kicks, &NoPreempt), Some(C0));
        // CPU0's pass consumes the message and finds the task gone: it was
        // migrated and is now an unconsumed Adopt aimed at CPU1.
        assert_eq!(rx[0].pop(&NoPreempt), Some(Msg::Retire(TaskKey(1))));
        assert!(!t.retire_node().in_flight(), "released on consume");
        assert!(t.transition(TaskState::Ready(C0), TaskState::InTransit(C1)));

        assert_eq!(chase(&t, &cpus, &kicks, &NoPreempt), Some(C1));
        assert_eq!(rx[1].pop(&NoPreempt), Some(Msg::Retire(TaskKey(1))));
        assert_eq!(&*kicks.0.lock().unwrap(), &[C0, C1]);
    }

    /// **The kill bit is set before the kick, and invariant 7's residual bound
    /// is the wrong way round without it.**
    ///
    /// `scheduler-core-spec.md` invariant 7 bounds a killed thread's remaining
    /// time in Ring 3 at one interrupt delivery: the exit boundary reads the
    /// bit with IF=0 immediately before the `iretq`, so a bit raised in that
    /// instant is missed — and what brings the thread back is this
    /// `Urgency::Preempt` kick. That argument holds only because the kick
    /// *follows* the bit. Issued first, the IPI could be consumed by a target
    /// still in Ring 0 with the bit invisible, leaving nothing in flight when
    /// the bit appears and the victim in Ring 3 until an unrelated tick.
    ///
    /// The spec and two code comments stated the order backwards while the code
    /// had it right, which is a proof of the bound's negation offered as a proof
    /// of the bound. This is the assertion that stops it being restated.
    ///
    /// **A host test and not a loom model**, deliberately: this is program
    /// order inside one thread — `claim_retire`'s locked read-modify-write, then
    /// `post`, then `kick` — and not a memory-ordering question between two.
    /// Loom would explore schedules that cannot reorder it and assert nothing
    /// this does not.
    #[test]
    fn the_kill_bit_is_set_before_the_kick_and_before_the_chase_kick() {
        let (cpus, mut rx) = world();
        let t = task(1, TaskState::Ready(C1));
        let kicks = SamplingKicks {
            task: t.clone(),
            seen: Mutex::new(Vec::new()),
        };

        assert_eq!(begin(&t).post(&cpus, &kicks, &NoPreempt), Some(C1));
        assert_eq!(rx[1].pop(&NoPreempt), Some(Msg::Retire(TaskKey(1))));

        // And the chase, which is the second site that kicks.
        assert!(t.transition(TaskState::Ready(C1), TaskState::InTransit(C0)));
        assert_eq!(chase(&t, &cpus, &kicks, &NoPreempt), Some(C0));
        assert_eq!(rx[0].pop(&NoPreempt), Some(Msg::Retire(TaskKey(1))));

        assert_eq!(
            &*kicks.seen.lock().unwrap(),
            &[true, true],
            "an IPI left before the kill bit was visible: invariant 7's residual \
             is a quantum, not an interrupt",
        );
    }

    #[test]
    fn retiring_a_dead_task_posts_nothing() {
        let (cpus, mut rx) = world();
        let kicks = Kicks::default();
        let t = task(1, TaskState::Blocked(C0));
        assert!(t.transition(TaskState::Blocked(C0), TaskState::Dead));
        assert_eq!(begin(&t).post(&cpus, &kicks, &NoPreempt), None);
        assert_eq!(rx[0].pop(&NoPreempt), None);
    }

    #[test]
    fn a_wake_and_a_retire_ride_distinct_nodes() {
        let (cpus, mut rx) = world();
        let kicks = Kicks::default();
        let t = task(1, TaskState::Blocked(C0));

        // The waker wins the claim; the retirer sets the kill bit. Both
        // messages are in flight at once, which is well-formed by
        // construction: they ride two distinct embedded nodes (spec §7.6).
        assert!(crate::waitq::wake_direct(
            &t,
            WakeCause::new(WakeReason::Woken),
            &cpus,
            &kicks,
            &NoPreempt,
        ));
        assert_eq!(t.claim_wake(), Claim::Lost, "the wake is already owned");
        assert_eq!(begin(&t).post(&cpus, &kicks, &NoPreempt), Some(C0));
        assert!(t.wake_node().in_flight() && t.retire_node().in_flight());
        assert_eq!(rx[0].pop(&NoPreempt), Some(Msg::Wake(TaskKey(1))));
        assert_eq!(rx[0].pop(&NoPreempt), Some(Msg::Retire(TaskKey(1))));
        assert_eq!(rx[0].pop(&NoPreempt), None);
    }
}
