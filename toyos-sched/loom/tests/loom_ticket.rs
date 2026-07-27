//! Loom: the two-phase wait handshake (spec §8.1, §8.2, §12).
//!
//! Three races, each a lost-wake window from the bug ledger (B3):
//! wake vs the `prepare_wait → block_on` commit, wake vs a cancel, and wake
//! vs a waiter's timeout — the last being the `Claim::Lost → try the next
//! waiter` rule, without which a `wake_one` racing a timeout is swallowed by
//! the corpse and the next waiter waits forever.

use loom::sync::atomic::{AtomicBool, Ordering};
use loom::sync::Arc;
use toyos_sched_loom::cpu::{CpuHandle, CpuHandles};
use toyos_sched_loom::mailbox::{mailbox, MailboxConsumer};
use toyos_sched_loom::model::{model, wait_list, LoomLock, Msg, PreemptModel, RemoteGuard, CPU0};
use toyos_sched_loom::task::{Claim, TaskKey, TaskShared, TaskState, WaitClass, WakeCause, WakeReason};
use toyos_sched_loom::waitq::{Cancelled, Commit, CurrentTask, WaitList, WaitQueue};
use toyos_sched_loom::model::Kicks;

type Queue = WaitQueue<Msg, LoomLock<WaitList<Msg>>>;

struct World {
    queue: Queue,
    cpus: CpuHandles<Msg>,
    kicks: Kicks,
    preempt: PreemptModel,
}

fn world() -> (Arc<World>, MailboxConsumer<Msg>) {
    let (tx, rx) = mailbox::<Msg>();
    (
        Arc::new(World {
            queue: WaitQueue::new(WaitClass::Pipe, wait_list()),
            cpus: CpuHandles::new(vec![CpuHandle::new(CPU0, tx)]),
            kicks: Kicks::new(),
            preempt: PreemptModel::new(),
        }),
        rx,
    )
}

fn task(key: u64) -> Arc<TaskShared<Msg>> {
    Arc::new(TaskShared::new(TaskKey(key), TaskState::Running(CPU0)))
}

fn drain(rx: &mut MailboxConsumer<Msg>, preempt: &PreemptModel) -> Vec<Msg> {
    let guard = preempt.disable();
    let mut msgs = Vec::new();
    while let Some(msg) = rx.pop(&guard) {
        msgs.push(msg);
    }
    msgs
}

/// The canonical blocking loop of spec §8.1 against a producer that makes the
/// condition true and wakes. No schedule may leave the waiter parked while
/// the condition holds — that is the check-then-block window, closed by
/// registering *before* the recheck.
#[test]
fn no_schedule_leaves_a_waiter_parked_with_the_condition_true() {
    model(|| {
        let (world, mut rx) = world();
        let ready = Arc::new(AtomicBool::new(false));
        let waiter = task(1);

        let producer = {
            let world = world.clone();
            let ready = ready.clone();
            loom::thread::spawn(move || {
                ready.store(true, Ordering::Release);
                world.queue.wake_one(
                    WakeCause::new(WakeReason::Woken),
                    &world.cpus,
                    &world.kicks,
                    &RemoteGuard,
                )
            })
        };

        // The waiter runs on CPU0 (this thread). Two iterations suffice: a
        // cancel can only be caused by the condition already being true.
        let mut parked = false;
        for _ in 0..2 {
            if ready.load(Ordering::Acquire) {
                break;
            }
            let ticket = world
                .queue
                .prepare_wait(&CurrentTask::new(&waiter, CPU0));
            if ready.load(Ordering::Acquire) {
                match ticket.cancel() {
                    Cancelled::Clean => continue,
                    // A waker claimed the registration: the wait is satisfied.
                    Cancelled::AlreadyWoken => break,
                }
            }
            match ticket.commit() {
                Commit::Parked(_) => {
                    parked = true;
                    break;
                }
                Commit::AlreadyWoken => break,
            }
        }

        let woken = producer.join().unwrap();
        let msgs = drain(&mut rx, &world.preempt);

        if parked {
            assert_eq!(
                waiter.state(),
                TaskState::WakeQueued(CPU0),
                "a parked waiter with the condition true must have been claimed",
            );
            assert_eq!(
                msgs,
                [Msg::Wake(TaskKey(1), WakeReason::Woken)],
                "the claim posts exactly one wake message",
            );
        } else {
            assert!(
                msgs.len() <= 1,
                "at most one wake message per claim: {msgs:?}",
            );
        }
        assert!(woken <= 1);
        assert!(world.queue.is_empty(), "no registration is left behind");
        assert!(!waiter.wake_node().in_flight() || !msgs.is_empty());
    });
}

/// A pre-park claim posts no message (spec §8.2 `Claim::PrePark`): the
/// waiter's own commit observes it and refuses to park, so no `Wake` is
/// queued and no switch happens.
#[test]
fn a_pre_park_claim_never_posts_a_message() {
    model(|| {
        let (world, mut rx) = world();
        let waiter = task(1);
        let ticket = world
            .queue
            .prepare_wait(&CurrentTask::new(&waiter, CPU0));

        let waker = {
            let world = world.clone();
            loom::thread::spawn(move || {
                world.queue.wake_one(
                    WakeCause::new(WakeReason::Woken),
                    &world.cpus,
                    &world.kicks,
                    &RemoteGuard,
                )
            })
        };

        let outcome = ticket.commit();
        let woken = waker.join().unwrap();
        let msgs = drain(&mut rx, &world.preempt);

        match outcome {
            Commit::Parked(_) => {
                // The waker either lost the race to us and posted a message
                // (we were already Blocked), or found nothing.
                assert_eq!(woken, msgs.len());
                assert!(msgs.iter().all(|m| *m == Msg::Wake(TaskKey(1), WakeReason::Woken)));
            }
            Commit::AlreadyWoken => {
                assert_eq!(woken, 1, "somebody must have claimed us");
                assert!(msgs.is_empty(), "a pre-park claim posts nothing: {msgs:?}");
                assert_eq!(waiter.state(), TaskState::Running(CPU0));
            }
        }
    });
}

/// §8.2's load-bearing retry: a `wake_one` that loses the claim to a
/// waiter's timeout must move on to the next waiter. Without the retry the
/// second waiter is stranded — the futex-storm shape.
#[test]
fn a_wake_racing_a_timeout_is_never_swallowed_by_the_corpse() {
    model(|| {
        let (world, mut rx) = world();
        let first = task(1);
        let second = task(2);
        for t in [&first, &second] {
            let ticket = world.queue.prepare_wait(&CurrentTask::new(t, CPU0));
            assert!(matches!(ticket.commit(), Commit::Parked(_)));
        }

        let timeout = {
            let world = world.clone();
            let first = first.clone();
            loom::thread::spawn(move || {
                // The home CPU's deadline fire: same claim CAS, no message.
                let won = first.claim_wake() == Claim::Parked(CPU0);
                if won {
                    world.queue.dequeue(&first);
                }
                won
            })
        };

        let woken = world.queue.wake_one(
            WakeCause::new(WakeReason::Timeout),
            &world.cpus,
            &world.kicks,
            &RemoteGuard,
        );
        let timeout_won = timeout.join().unwrap();
        let msgs = drain(&mut rx, &world.preempt);

        assert_eq!(woken, 1, "a live waiter must always be found");
        let expected = if timeout_won { TaskKey(2) } else { TaskKey(1) };
        assert_eq!(
            msgs,
            [Msg::Wake(expected, WakeReason::Timeout)],
            "the wake must reach a live waiter, never a corpse \
             (timeout_won={timeout_won})",
        );

        // Tidy up so the surviving registration does not outlive the model.
        for t in [&first, &second] {
            world.queue.dequeue(t);
        }
        assert!(world.kicks.count() <= 1);
    });
}

/// Cancel vs wake: exactly one of them wins, and the loser reports it.
#[test]
fn cancel_and_wake_agree_on_who_won() {
    model(|| {
        let (world, mut rx) = world();
        let waiter = task(1);
        let ticket = world
            .queue
            .prepare_wait(&CurrentTask::new(&waiter, CPU0));

        let waker = {
            let world = world.clone();
            loom::thread::spawn(move || {
                world.queue.wake_one(
                    WakeCause::new(WakeReason::Woken),
                    &world.cpus,
                    &world.kicks,
                    &RemoteGuard,
                )
            })
        };

        let cancelled = ticket.cancel();
        let woken = waker.join().unwrap();
        let msgs = drain(&mut rx, &world.preempt);

        match cancelled {
            Cancelled::Clean => assert_eq!(woken, 0, "we withdrew before any claim"),
            Cancelled::AlreadyWoken => assert_eq!(woken, 1, "the waker claimed us"),
        }
        assert!(msgs.is_empty(), "a pre-park claim posts nothing: {msgs:?}");
        assert_eq!(waiter.state(), TaskState::Running(CPU0));
        assert!(world.queue.is_empty());
    });
}
