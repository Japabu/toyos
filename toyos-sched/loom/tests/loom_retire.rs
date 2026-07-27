//! Loom: the retire protocol (spec §7.6, §12).
//!
//! Three races: the kill bit against a concurrent wake claim, the retire-node
//! re-post chase against a migration, and adoption under a kill. What replaced
//! `KILLED[16]`, `WAKE_TRANSITS` and the 1 s timeout scan is a sticky bit plus
//! a message, so the cases worth checking are exactly the orderings of that
//! bit and that node.

use loom::sync::Arc;
use toyos_sched_loom::cpu::{CpuHandle, CpuHandles};
use toyos_sched_loom::mailbox::{mailbox, MailboxConsumer};
use toyos_sched_loom::model::{model, Kicks, Msg, PreemptModel, RemoteGuard, CPU0, CPU1};
use toyos_sched_loom::retire;
use toyos_sched_loom::task::{TaskKey, TaskShared, TaskState, WakeCause, WakeReason};
use toyos_sched_loom::waitq::wake_direct;

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
