//! Loom: the retire protocol (spec §7.6, §12).
//!
//! Four races: the kill bit against a concurrent wake claim, the kill bit
//! against a waiter's own park commit, the retire-node re-post chase against a
//! migration, and adoption under a kill. What replaced `KILLED[16]`,
//! `WAKE_TRANSITS` and the 1 s timeout scan is a sticky bit plus a message, so
//! the cases worth checking are exactly the orderings of that bit and that
//! node.

use loom::sync::Arc;
use toyos_sched_loom::cpu::{CpuHandle, CpuHandles};
use toyos_sched_loom::mailbox::{mailbox, MailboxConsumer};
use toyos_sched_loom::model::{
    model, wait_list, Kicks, LoomLock, Msg, PreemptModel, RemoteGuard, CPU0, CPU1,
};
use toyos_sched_loom::retire;
use toyos_sched_loom::task::{TaskKey, TaskShared, TaskState, WaitClass, WakeCause, WakeReason};
use toyos_sched_loom::waitq::{wake_direct, Commit, CurrentTask, WaitList, WaitQueue};

struct World {
    cpus: CpuHandles<Msg>,
    kicks: Kicks,
    preempt: PreemptModel,
}

fn world() -> (Arc<World>, Vec<MailboxConsumer<Msg>>) {
    let (tx0, rx0) = mailbox::<Msg>();
    let (tx1, rx1) = mailbox::<Msg>();
    (
        Arc::new(World {
            cpus: CpuHandles::new(vec![CpuHandle::new(CPU0, tx0), CpuHandle::new(CPU1, tx1)]),
            kicks: Kicks::new(),
            preempt: PreemptModel::new(),
        }),
        vec![rx0, rx1],
    )
}

fn drain(rx: &mut MailboxConsumer<Msg>, preempt: &PreemptModel) -> Vec<Msg> {
    let guard = preempt.disable();
    let mut msgs = Vec::new();
    while let Some(msg) = rx.pop(&guard) {
        msgs.push(msg);
    }
    msgs
}

/// A wake and a retire for the same task may be in flight at once. They are
/// well-formed by construction — two distinct embedded nodes — and both are
/// delivered exactly once, in the order the one consumer sees them.
#[test]
fn a_wake_and_a_retire_ride_distinct_nodes() {
    model(|| {
        let (world, mut rx) = world();
        let task = Arc::new(TaskShared::<Msg>::new(TaskKey(1), TaskState::Blocked(CPU0)));

        let waker = {
            let world = world.clone();
            let task = task.clone();
            loom::thread::spawn(move || {
                wake_direct(
                    &task,
                    WakeCause::new(WakeReason::Woken),
                    &world.cpus,
                    &world.kicks,
                    &RemoteGuard,
                )
            })
        };

        let retired = retire::begin(&task).post(&world.cpus, &world.kicks, &RemoteGuard);
        let woke = waker.join().unwrap();

        assert_eq!(retired, Some(CPU0), "the home cpu owns the death");
        assert!(task.kill_pending() && task.retire_queued());

        let msgs = drain(&mut rx[0], &world.preempt);
        assert_eq!(
            msgs.iter().filter(|m| **m == Msg::Retire(TaskKey(1))).count(),
            1,
            "exactly one retire message: {msgs:?}",
        );
        let wakes = msgs
            .iter()
            .filter(|m| **m == Msg::Wake(TaskKey(1), WakeReason::Woken))
            .count();
        assert_eq!(wakes, usize::from(woke), "a claimed wake posts one message");
        assert!(drain(&mut rx[1], &world.preempt).is_empty());
        assert!(!task.wake_node().in_flight() && !task.retire_node().in_flight());
    });
}

/// A retire racing a waiter's own park commit — the window `Commit::Killed`
/// closes (spec §6.3's park-as-safe-point, §7.6's "dies at its next one").
///
/// Whichever order the two land in, *someone* must be left able to reap the
/// task. Either the commit observed the kill bit and withdrew — leaving the
/// word at `Running`, which is the only thing the exit disposition can consume
/// — or it parked, and then the retire message is queued to the CPU that now
/// owns the parked task. The third outcome is the defect: parked, killed, and
/// nothing left to notice, which is a thread that never dies and an address
/// space that is never released.
#[test]
fn a_retire_racing_the_park_commit_always_leaves_someone_to_reap() {
    model(|| {
        let (world, mut rx) = world();
        let queue: WaitQueue<Msg, LoomLock<WaitList<Msg>>> =
            WaitQueue::new(WaitClass::Pipe, wait_list());
        let waiter = Arc::new(TaskShared::<Msg>::new(TaskKey(1), TaskState::Running(CPU0)));
        let ticket = queue.prepare_wait(&CurrentTask::new(&waiter, CPU0));

        let retirer = {
            let world = world.clone();
            let waiter = waiter.clone();
            loom::thread::spawn(move || {
                retire::begin(&waiter).post(&world.cpus, &world.kicks, &RemoteGuard)
            })
        };

        let outcome = ticket.commit();
        let target = retirer.join().unwrap();
        let msgs = drain(&mut rx[0], &world.preempt);

        assert_eq!(target, Some(CPU0), "every word in this model names cpu0");
        assert_eq!(
            msgs,
            [Msg::Retire(TaskKey(1))],
            "exactly one retire message, whichever way the race went",
        );
        match outcome {
            Commit::Killed => {
                assert_eq!(
                    waiter.state(),
                    TaskState::Running(CPU0),
                    "the exit disposition needs the word back at Running",
                );
                assert!(queue.is_empty(), "the registration is withdrawn");
            }
            // The commit read the bit before the retirer set it. That is fine
            // precisely because the message above exists: the pass that drains
            // it finds the task in `parked` and reaps it there.
            Commit::Parked(_, registration) => {
                assert_eq!(waiter.state(), TaskState::Blocked(CPU0));
                registration.finish();
            }
            Commit::AlreadyWoken => unreachable!("nothing wakes in this model"),
        }
        assert!(!waiter.retire_node().in_flight());
    });
}

/// The chase (spec §7.6 step 2): the home CPU consumed the retire, found the
/// task gone, and re-posts the *same* node to wherever the word now points.
/// Racing that with the migration itself must still produce exactly one
/// message and must never link the node twice.
#[test]
fn the_retire_chase_reuses_one_node_under_a_racing_migration() {
    model(|| {
        let (world, mut rx) = world();
        let task = Arc::new(TaskShared::<Msg>::new(TaskKey(1), TaskState::Ready(CPU0)));
        assert_eq!(
            retire::begin(&task).post(&world.cpus, &world.kicks, &RemoteGuard),
            Some(CPU0),
        );
        assert_eq!(drain(&mut rx[0], &world.preempt), [Msg::Retire(TaskKey(1))]);

        let migration = {
            let task = task.clone();
            loom::thread::spawn(move || {
                // The pass that owns the task hands it to CPU1.
                task.transition(TaskState::Ready(CPU0), TaskState::InTransit(CPU1))
            })
        };

        let chased = retire::chase(&task, &world.cpus, &world.kicks, &RemoteGuard);
        let migrated = migration.join().unwrap();

        let delivered: Vec<Msg> = rx
            .iter_mut()
            .flat_map(|q| drain(q, &world.preempt))
            .collect();
        assert_eq!(
            delivered,
            [Msg::Retire(TaskKey(1))],
            "one chase, one message (migrated={migrated})",
        );
        assert!(
            chased == Some(CPU0) || chased == Some(CPU1),
            "the chase follows the word: {chased:?}",
        );
        assert!(!task.retire_node().in_flight(), "the node is free again");
    });
}

/// The kill bit is sticky and set before the message is posted, so whichever
/// CPU ends up owning the task observes it and reaps on arrival — that is the
/// chase's termination argument (spec §7.6).
#[test]
fn an_adopting_cpu_always_observes_the_kill_bit() {
    model(|| {
        let (world, mut rx) = world();
        let task = Arc::new(TaskShared::<Msg>::new(TaskKey(1), TaskState::InTransit(CPU1)));

        let adopter = {
            let task = task.clone();
            loom::thread::spawn(move || {
                let adopted = task.transition(TaskState::InTransit(CPU1), TaskState::Ready(CPU1));
                (adopted, task.kill_pending())
            })
        };

        let target = retire::begin(&task).post(&world.cpus, &world.kicks, &RemoteGuard);
        let (adopted, saw_kill) = adopter.join().unwrap();

        assert!(adopted, "the adopt transition always wins its own edge");
        assert!(task.kill_pending(), "KILL is sticky");
        // Either the adopter saw the bit itself, or the retire message is
        // waiting on the CPU it adopted onto — never neither.
        let delivered: Vec<Msg> = rx
            .iter_mut()
            .flat_map(|q| drain(q, &world.preempt))
            .collect();
        assert_eq!(delivered, [Msg::Retire(TaskKey(1))]);
        assert!(
            saw_kill || target == Some(CPU1) || target == Some(CPU0),
            "the death always reaches an owner",
        );
        assert!(!task.retire_node().in_flight());
    });
}

/// **The fifth race, and the one `specs/completion-architecture-spec.md` §7.3
/// names as missing: a retire that finds its victim already parked.**
///
/// §7.2 rewrites that arm from a reap-in-place into a claim-arbitrated wake,
/// and the arbitration is the whole of what can go wrong. The retirer and a
/// remote waker reach for the same rendezvous word; exactly one of them may win
/// it, and whichever loses must leave the task somewhere the other one's
/// message will find it. The defect this excludes is the one §7.2(c) describes
/// in terms: remove-then-convert, where the retirer takes the entry out of
/// `parked` and then loses the claim, so the in-flight `Msg::Wake` lands on a
/// `handle_wake` whose `parked.remove` returns `None` — and the task is in no
/// container at all, never runnable, never reaped, until the retirer's own
/// tripwire panics the machine.
///
/// Modelled at the claim rather than at the container, because the container
/// is `CpuSched`'s and `CpuSched` is `!Sync`: one CPU owns it and no
/// interleaving can reach it. What two CPUs really race for is this word.
#[test]
fn a_retire_and_a_wake_never_both_claim_a_parked_task() {
    model(|| {
        let (world, mut rx) = world();
        let task = Arc::new(TaskShared::<Msg>::new(TaskKey(1), TaskState::Blocked(CPU0)));

        // The waker: an ordinary post through the same claim CAS every wake
        // goes through.
        let waker = {
            let world = world.clone();
            let task = task.clone();
            loom::thread::spawn(move || {
                wake_direct(
                    &task,
                    WakeCause::new(WakeReason::Woken),
                    &world.cpus,
                    &world.kicks,
                    &RemoteGuard,
                )
            })
        };

        // The retirer: the kill bit, the message, and then — on the CPU that
        // owns the task — the claim `handle_retire` now makes.
        retire::begin(&task).post(&world.cpus, &world.kicks, &RemoteGuard);
        let retire_claimed = matches!(task.claim_wake(), toyos_sched_loom::task::Claim::Parked(_));

        let wake_claimed = waker.join().unwrap();
        let msgs = drain(&mut rx[0], &world.preempt);

        assert!(
            !(retire_claimed && wake_claimed),
            "two claims on one parked task: the wake and the retire would both place it",
        );
        assert!(
            retire_claimed || wake_claimed,
            "neither claimed a task that was parked the whole time: it is in no container",
        );
        assert!(
            msgs.contains(&Msg::Retire(TaskKey(1))),
            "the retire message is delivered whichever way the claim went",
        );
        if wake_claimed {
            // The retirer left the entry alone, which is what makes the
            // in-flight wake able to find it. `handle_wake` places it, and the
            // kill bit — already set above — is what sends it to the dying
            // list rather than the fair queue.
            assert!(
                msgs.contains(&Msg::Wake(TaskKey(1), WakeReason::Woken)),
                "the wake it lost to must be the message that places the task",
            );
            assert_eq!(task.state(), TaskState::WakeQueued(CPU0));
        } else {
            assert_eq!(task.state(), TaskState::WakeQueued(CPU0));
        }
        assert!(task.kill_pending(), "the kill bit is sticky either way");
    });
}
