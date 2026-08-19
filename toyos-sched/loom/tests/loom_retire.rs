//! Loom: the retire protocol (spec §7.6, §12).
//!
//! Six races: the kill bit against a concurrent wake claim, the kill bit
//! against a waiter's own park commit, the retire-node re-post chase against a
//! migration, adoption under a kill, the two claims on one parked task, and —
//! last, and the only one that runs a whole `CpuSched` — what
//! `handle_retire` *does* with the answer it gets. What replaced `KILLED[16]`,
//! `WAKE_TRANSITS` and the 1 s timeout scan is a sticky bit plus a message, so
//! the cases worth checking are exactly the orderings of that bit and that
//! node.

use loom::sync::Arc;
use toyos_sched_loom::cpu::{CpuHandle, CpuHandles, CpuSched, Env, RunToken, SchedPass};
use toyos_sched_loom::fair::{FairShare, Frontier, ShareState};
use toyos_sched_loom::hw::{CpuId, Hw, Kicker, Machine, Nanos, TraceEvent};
use toyos_sched_loom::mailbox::{mailbox, MailboxConsumer, Urgency};
use toyos_sched_loom::model::{
    model, wait_list, IrqGuard, Kicks, LoomLock, Msg, PreemptModel, RemoteGuard, CPU0, CPU1,
};
use toyos_sched_loom::retire;
use toyos_sched_loom::task::{
    RtState, SchedPayload, TaskAccounting, TaskBuilder, TaskKey, TaskShared, TaskState, WaitClass,
    WakeCause, WakeReason,
};
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

/// **The fifth race, and the one the completion work names as missing: a
/// retire that finds its victim already parked.**
///
/// The cancellable kill rewrites that arm from a reap-in-place into a
/// claim-arbitrated wake, and the arbitration is the whole of what can go
/// wrong. The retirer and a remote waker reach for the same rendezvous word;
/// exactly one of them may win it, and whichever loses must leave the task
/// somewhere the other one's message will find it. The defect this excludes
/// is remove-then-convert: the retirer takes the entry out of `parked` and
/// then loses the claim, so the in-flight `Msg::Wake` lands on a
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

// The sixth model runs a whole `CpuSched`, so it needs the three things a pass
// is built against: a payload, a hardware surface, and the shared half of a CPU.

/// The smallest payload a `CpuSched` can be built over: no address space, no
/// saved context, and loom's mutex as the per-process share cell.
struct Payload;

impl SchedPayload for Payload {
    type Ctx = ();
    type ShareLock = LoomLock<ShareState>;
}

/// The full message set (`crate::msg::Msg`), as opposed to the reduced [`Msg`]
/// the primitive models use: a pass consumes `Adopt` and `Retire` too.
type TaskMsg = toyos_sched_loom::msg::Msg<Payload>;

/// An `Hw` that records nothing. Every question this model asks is about which
/// of `CpuSched`'s own containers the task is in, and `CpuSched` answers those
/// itself; a recorder would only add state loom has to explore.
struct Silent;

impl Kicker for Silent {
    fn kick(&self, _target: CpuId) {}
}

impl Machine for Silent {
    type IrqGuard = ();
    fn now(&self) -> Nanos {
        NOW
    }
    fn set_timer(&self, _deadline: Nanos) {}
    fn stop_timer(&self) {}
    fn irq_guard(&self) {}
    fn halt(&self) {}
    fn need_resched(&self, _cpu: CpuId) {}
    fn trace(&self, _ev: TraceEvent) {}
}

impl Hw for Silent {
    type Payload = Payload;
    #[allow(unsafe_code)] // the declaration is unsafe; this body does nothing
    unsafe fn switch(&self, _token: RunToken<Payload>) {}
    fn release(&self, _key: TaskKey, _payload: Payload, _acct: TaskAccounting) {}
}

/// Everything a pass touches that is **not** the `CpuSched`. The waker thread
/// gets a handle to this and to nothing else, which is the whole of §6.1: a
/// remote CPU may post and ring, and there is no second way in.
struct Owner {
    cpus: CpuHandles<TaskMsg>,
    hw: Silent,
    frontier: Frontier,
}

/// The instant every pass in the model runs at. Constant on purpose: a quantum
/// that never expires keeps `preempt_if_due` out of the interleavings, which
/// have nothing to do with it.
const NOW: Nanos = Nanos(1_000);
/// The task the retire and the wake race for.
const KEY: TaskKey = TaskKey(1);
/// The task that keeps the CPU busy — see the model's own note on why an idle
/// CPU cannot be modelled here.
const OTHER: TaskKey = TaskKey(2);

/// **The sixth model, and the first that executes a line of `CpuSched`.**
///
/// The model above races the two claims and stops at the CAS — it calls
/// `TaskShared::claim_wake` where `handle_retire` calls it. That states the
/// arbitration is exclusive and says nothing about what the retirer *does* with
/// the answer, and the gap was measurable: a `panic!()` at the top of
/// `handle_retire` left all thirteen models green, so no model reached the arm
/// §7.2(c) is about.
///
/// This one drives it. The setup is an ordinary life: adopt, dispatch, park.
/// Then a remote waker and a retirer reach for the same parked task, and the
/// CPU that owns it runs the passes that consume whatever arrives. `CpuSched`
/// is `!Sync` and stays on its own thread, exactly as a CPU's scheduler state
/// does; what crosses is the message.
///
/// The property is §7.2(c) itself: **whichever way the claim goes, the task
/// ends up in the dying list.** If the retirer wins, its own wake places it
/// there; if it loses, the waker's `Msg::Wake` is in flight to this same CPU
/// and `handle_wake` places it — but only if the retirer left the entry in
/// `parked`. Remove-then-convert reds this model: the entry is gone, the wake
/// lands on a `parked.remove` that returns `None`, and the task is in no
/// container at all.
///
/// **The CPU is deliberately never idle.** `OTHER` runs throughout, and it is
/// not scenery: an idle `SchedPass` ends in `try_sleep`, whose `Err(())` retry
/// re-drains and decides again, and a producer that loom leaves suspended
/// mid-push holds `is_empty()` false for as long as loom cares to leave it
/// there — so the loop is unbounded in the model and 2 instructions on the
/// machine (N3 bounds the torn-push window; `loom_mailbox.rs` is where *that*
/// is checked). Modelling this arm therefore means modelling a busy CPU, which
/// is also the case the arm exists for.
#[test]
fn the_retire_arm_never_loses_a_parked_task_to_a_racing_wake() {
    model(|| {
        let (tx, rx) = mailbox::<TaskMsg>();
        let owner = Arc::new(Owner {
            cpus: CpuHandles::new(vec![CpuHandle::new(CPU0, tx)]),
            hw: Silent,
            frontier: Frontier::new(),
        });
        let mut cpu = CpuSched::<Payload>::new(CPU0, rx, ());
        // A pass runs from the IRQ-exit path, which cannot be preempted, so
        // `IrqGuard` is its guard (N3). The waker is a *remote* CPU and carries
        // its own — the exclusion that would matter here is between a pass and
        // a preempt-disabled section on this same CPU, and there is no such
        // section in this model.
        let guard = IrqGuard;
        let env = Env {
            hw: &owner.hw,
            cpus: &owner.cpus,
            frontier: &owner.frontier,
            preempt: &guard,
            steal: false,
        };
        let pass = |cpu: &mut CpuSched<Payload>| {
            let _ = SchedPass::begin(cpu, env, NOW).dispose_none().finish();
        };
        // Spawn placement is a message, never a reach into the queue (§9.4).
        let spawn = |key: TaskKey| {
            let share = Arc::new(FairShare::new(LoomLock::new(ShareState::NonRunnable {
                lag: 0,
            })));
            let task = TaskBuilder {
                key,
                share,
                ctx: (),
                ext: Payload,
                rt: RtState::default(),
            }
            .build(CPU0, NOW);
            let shared = task.shared().clone();
            let _ = owner.cpus.get(CPU0).post_owned(
                TaskMsg::Adopt { task },
                TaskMsg::adopt_node,
                Urgency::Normal,
                &guard,
            );
            shared
        };

        let shared = spawn(KEY);
        pass(&mut cpu);
        assert_eq!(cpu.running().map(|t| t.key()), Some(KEY), "adopted and picked");

        // It blocks on something — the state §7.1 calls the arm that matters —
        // and the pick hands the CPU to `OTHER`, adopted in the same pass.
        let other = spawn(OTHER);
        let queue: WaitQueue<TaskMsg, LoomLock<WaitList<TaskMsg>>> =
            WaitQueue::new(WaitClass::Pipe, wait_list());
        let ticket = {
            let current = cpu.current_task().expect("the task is running");
            queue.prepare_wait(&current)
        };
        let (committed, registration) = match ticket.commit() {
            Commit::Parked(committed, registration) => (committed, registration),
            _ => unreachable!("nothing has touched this task yet"),
        };
        let _ = SchedPass::begin(&mut cpu, env, NOW)
            .dispose_block(committed, None)
            .finish();
        assert_eq!(shared.state(), TaskState::Blocked(CPU0));
        assert_eq!(cpu.running().map(|t| t.key()), Some(OTHER), "the CPU has work");

        // The race: a waker on another CPU, and this CPU's own retirer.
        let waker = {
            let owner = owner.clone();
            let shared = shared.clone();
            loom::thread::spawn(move || {
                wake_direct(
                    &shared,
                    WakeCause::new(WakeReason::Woken),
                    &owner.cpus,
                    &owner.hw,
                    &RemoteGuard,
                )
            })
        };
        retire::begin(&shared).post(&owner.cpus, &owner.hw, &RemoteGuard);

        // The pass that consumes the retire. This is the arm under test.
        pass(&mut cpu);
        let woke = waker.join().unwrap();
        // And the one that consumes a wake posted after that drain. Joining
        // first is what makes the second pass conclusive: every message either
        // side will ever post has been posted by now.
        pass(&mut cpu);

        assert!(
            cpu.dying().any(|t| t.key() == KEY),
            "the task is in no container: the retire {} the claim and nothing placed it",
            if woke { "lost" } else { "won" },
        );
        assert!(
            cpu.parked().next().is_none(),
            "a retire that lost the claim leaves the entry for the wake to find, \
             and the wake takes it out of `parked`",
        );
        assert!(
            !cpu.rq().keys().any(|key| key == KEY),
            "a killed task is placed in the dying list, never in the fair band",
        );
        assert!(shared.kill_pending(), "the kill bit is sticky either way");

        // Wind both tasks down by the only death there is, so the model leaves
        // nothing alive: `OTHER` exits, the pick takes the corpse off the dying
        // list, it dies by its own `die`, and a last pass frees the zombie.
        registration.finish();
        let _ = SchedPass::begin(&mut cpu, env, NOW).dispose_exit().finish();
        assert_eq!(other.state(), TaskState::Dead);
        assert_eq!(cpu.running().map(|t| t.key()), Some(KEY), "the unwind runs");
        let _ = SchedPass::begin(&mut cpu, env, NOW).dispose_exit().finish();
        pass(&mut cpu);
        assert_eq!(shared.state(), TaskState::Dead);
    });
}
