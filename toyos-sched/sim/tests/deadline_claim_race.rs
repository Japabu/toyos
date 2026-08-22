//! A wake claimed between a pass's mailbox drain and its deadline fire.
//!
//! `SchedPass::begin` runs `drain` and `fire_deadlines` back to back with no
//! step boundary between them, so the explorer cannot place a remote claim
//! there: `fire_deadlines`' `Claim::Lost` arm is unreachable in every scenario
//! the simulator can generate. On hardware the window is two instructions of a
//! remote CPU — `TaskShared::claim_wake` and the `Msg::Wake` post that follows
//! it in `waitq::deliver_wake` — and this test is that pair with a whole pass
//! executed in between, which is the only way to say it.
//!
//! What it asserts is invariant T (spec §8.4) across that window: a CPU that
//! holds a parked task with a pending deadline has its timer armed for it. The
//! state it constructs is the one `kernel/src/sched/dump.rs` cannot describe —
//! a machine halted with a parked thread and a stopped timer reports
//! `1 pending, 0 OVERDUE`, which is what health looks like.
//!
//! It fails by aborting, like `scenarios::old_preemptible_window`: a task and
//! a wait registration are linear values with drop bombs, so unwinding out of
//! a failed assertion sets them off. The first line printed is the verdict.

use std::sync::Arc as StdArc;

use toyos_sched::cpu::{Action, Balance, CpuHandle, CpuHandles, CpuSched, Env, SchedPass};
use toyos_sched::fair::{FairShare, Frontier, ShareState};
use toyos_sched::hw::{CpuId, Kicker, Nanos};
use toyos_sched::mailbox::{mailbox, Kick, Urgency};
use toyos_sched::msg::Msg;
use toyos_sched::sync::Arc;
use toyos_sched::task::{
    Claim, RtState, TaskBuilder, TaskKey, TaskShared, WaitClass, WakeCause, WakeReason,
};
use toyos_sched::waitq::{Commit, WaitList, WaitQueue};

use toyos_sched_sim::hw_impl::SimHw;
use toyos_sched_sim::msg::{SimMsg, SimQueue};
use toyos_sched_sim::payload::{MockAddressSpace, SimCtx, SimPayload, SimPreempt, StdLock};

const CPU0: CpuId = CpuId(0);
const KEY: TaskKey = TaskKey(1);
/// Well inside the quantum, so every arming decision this test observes is
/// about the deadline and never about a timeslice running out.
const DEADLINE: Nanos = Nanos(5_000_000);

struct Machine1 {
    hw: SimHw,
    handles: CpuHandles<SimMsg>,
    frontier: Frontier,
    queue: SimQueue,
}

impl Machine1 {
    fn env(&self) -> Env<'_, SimHw, SimPreempt> {
        Env {
            hw: &self.hw,
            cpus: &self.handles,
            frontier: &self.frontier,
            preempt: &SimPreempt,
            balance: Balance::None,
        }
    }
}

/// Run one ordinary pass at `now`. The action is discarded: this test asserts
/// on `CpuSched`'s own view, and `SimHw`'s switch and halt bookkeeping serves
/// invariants — I11, I2 — that are not what is being measured here.
fn pass(m: &Machine1, cpu: &mut CpuSched<SimPayload>, now: Nanos) {
    m.hw.enter_pass(CPU0, now);
    let action = SchedPass::begin(cpu, m.env(), now).dispose_none().finish();
    drop_action(action);
    m.hw.leave_pass();
}

fn drop_action(action: Action<SimPayload>) {
    match action {
        Action::Run(_) | Action::Resume | Action::Idle(_) => {}
    }
}

/// One CPU, one task, running. The task arrives the way every task does — as
/// an `Adopt` in the mailbox — so nothing here bypasses a state transition.
fn boot() -> (Machine1, CpuSched<SimPayload>, Arc<TaskShared<SimMsg>>) {
    let (tx, rx) = mailbox::<SimMsg>();
    let m = Machine1 {
        hw: SimHw::new(1),
        handles: CpuHandles::new(vec![CpuHandle::new(CPU0, tx)]),
        frontier: Frontier::new(),
        queue: WaitQueue::new(WaitClass::Io, StdLock::new(WaitList::new())),
    };
    let mut cpu = CpuSched::new(CPU0, rx, SimCtx::default());

    let task = TaskBuilder {
        key: KEY,
        share: Arc::new(FairShare::new(StdLock::new(ShareState::NonRunnable {
            lag: 0,
        }))),
        ctx: SimCtx { key: Some(KEY) },
        ext: SimPayload {
            key: KEY,
            process: 0,
            address_space: StdArc::new(MockAddressSpace { process: 0 }),
        },
        rt: RtState {
            permanent: false,
            inherited: None,
            lends: 0,
        },
    }
    .build(CPU0, Nanos::ZERO);
    let shared = task.shared().clone();
    let kick = m.handles.get(CPU0).post_owned(
        Msg::Adopt { task },
        Msg::adopt_node,
        Urgency::Normal,
        &SimPreempt,
    );
    if kick == Kick::Send {
        m.hw.kick(CPU0);
    }

    pass(&m, &mut cpu, Nanos::ZERO);
    assert_eq!(
        cpu.running().map(|t| t.key()),
        Some(KEY),
        "the adopted task did not reach the CPU",
    );
    (m, cpu, shared)
}

/// A task is a linear value: it has to leave through the protocol, or dropping
/// the CPU that holds it sets off its drop bomb over whatever the verdict was.
/// Two passes, because the first cannot free the context it is standing on.
fn teardown(m: &Machine1, cpu: &mut CpuSched<SimPayload>, now: Nanos) {
    if cpu.running().is_some() {
        m.hw.enter_pass(CPU0, now);
        drop_action(SchedPass::begin(cpu, m.env(), now).dispose_exit().finish());
        m.hw.leave_pass();
    }
    pass(m, cpu, now.after(1_000));
}

/// The second half of `waitq::deliver_wake`, on its own. Splitting it from the
/// claim is what lets a pass run between the two.
fn deliver(m: &Machine1, shared: &Arc<TaskShared<SimMsg>>, cause: WakeCause) {
    let slot = shared
        .wake_node()
        .claim()
        .expect("the wake claim admits one poster");
    let kick = m
        .handles
        .get(CPU0)
        .post(slot, Msg::Wake { key: KEY, cause }, cause.urgency(), &SimPreempt);
    if kick == Kick::Send {
        m.hw.kick(CPU0);
    }
}

#[test]
fn a_claim_between_the_drain_and_the_fire_leaves_the_timer_armed_for_what_is_parked() {
    let (m, mut cpu, shared) = boot();

    let registration = {
        m.hw.enter_pass(CPU0, Nanos::ZERO);
        let ticket = {
            let current = cpu.current_task().expect("a task is running");
            m.queue.prepare_wait(&current)
        };
        let pass = SchedPass::begin(&mut cpu, m.env(), Nanos::ZERO);
        let Commit::Parked(committed, registration) = ticket.commit() else {
            panic!("nothing had claimed the task: the commit must park it");
        };
        drop_action(pass.dispose_block(committed, Some(DEADLINE)).finish());
        m.hw.leave_pass();
        registration
    };
    assert_eq!(
        cpu.armed(),
        Some(DEADLINE),
        "a park with a deadline must arm the timer for it",
    );

    // The remote CPU's first instruction. Its `Msg::Wake` post is the second,
    // and it has not happened yet.
    assert_eq!(
        shared.claim_wake(),
        Claim::Parked(CPU0),
        "the parked task's wake must be claimable",
    );

    // The pass that lands in between, and whose `fire_deadlines` finds the
    // deadline due and loses the claim.
    let after = DEADLINE.after(1_000);
    pass(&m, &mut cpu, after);
    let armed = cpu.armed();
    let owed = cpu.parked().filter_map(|p| p.deadline()).min();

    // The remote CPU's second instruction, and the recovery it owes.
    deliver(&m, &shared, WakeCause::new(WakeReason::Woken));
    pass(&m, &mut cpu, after.after(1_000));
    let ran_again = cpu.running().map(|t| t.key());
    registration.finish();
    teardown(&m, &mut cpu, after.after(2_000));

    assert_eq!(
        armed, owed,
        "cpu0 holds a task parked on a deadline of {owed:?} with its timer at {armed:?}: \
         nothing will fire it, and nothing that reads the CPU can tell that from health",
    );
    assert_eq!(
        owed, None,
        "the timeout is superseded — no later claim can succeed — so the CPU must stop \
         reporting it as pending",
    );
    assert_eq!(
        ran_again,
        Some(KEY),
        "the wake behind the lost claim never reached the task",
    );
}
