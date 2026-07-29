//! The virtual machine — spec §10.2.
//!
//! Virtual CPUs are not host threads (spec §13.13): the VM holds a set of
//! *enabled steps* and the explorer picks one per iteration. That is what
//! makes a run reproducible from its decision sequence alone, and what lets a
//! failure be shrunk by deleting decisions.
//!
//! Time is a single virtual clock advanced by execution steps. A CPU with a
//! task loaded accrues busy time for every advance, whichever CPU caused it —
//! which is what a real multiprocessor does, and what makes invariant I7's
//! conservation law exact.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc as StdArc;

use toyos_sched::cpu::{Action, CpuHandle, CpuSched, Env, SchedPass};
use toyos_sched::fair::{FairShare, Frontier, ShareState};
use toyos_sched::hw::{CpuId, Hw, Kicker, Machine, Nanos};
use toyos_sched::mailbox::{mailbox, Kick, Urgency};
use toyos_sched::msg::Msg;
use toyos_sched::retire;
use toyos_sched::sync::Arc;
use toyos_sched::task::{
    ReadyTask, RtState, TaskAccounting, TaskBuilder, TaskKey, TaskShared, TaskState, WaitClass,
    WakeCause, WakeReason,
};
use toyos_sched::waitq::{
    Cancelled, Commit, CommittedTicket, Registration, WaitList, WaitQueue, WaitTicket,
};

use crate::choice::ChoiceStream;
use crate::hw_impl::SimHw;
use crate::msg::{SimHandles, SimMsg, SimQueue};
use crate::payload::{
    MockAddressSpace, SimCtx, SimPayload, SimPreempt, SimShareLock, SimWaitList, StdLock,
};
use crate::workload::{BlockShape, Op, ParkShape, Protocol, Scenario, Script, WindowShape};

/// How finely a `Run(ns)` op is chopped. Small enough that a 10 ms quantum
/// expires in the middle of a run — the interesting case — without making
/// long workloads absurdly many steps.
pub const RUN_CHUNK_NS: u64 = 1_000_000;

/// How long a targeted IPI may go undelivered. Modelled, not assumed: past
/// this point the target's `Run` steps stop being enabled, so the explorer
/// cannot starve an interrupt the way it can starve a voluntary step. Real
/// hardware has the same property, and invariant I4's bound depends on it.
pub const IPI_LATENCY_NS: u64 = 200_000;

/// One waitable object: a queue plus the condition its waiters test. The
/// token count is what makes a lost wake *observable* — a waiter parked while
/// its queue holds a token is a wake that went missing.
pub struct QueueState {
    pub queue: SimQueue,
    pub tokens: Cell<u32>,
    /// A boost the producer left for whoever consumes next. Spec §8.5's
    /// second bullet: a client that was *not* blocked at signal time cannot
    /// be handed the window through a wake cause, so the object carries it
    /// and the consume path picks it up.
    pub boost_until: Cell<Option<Nanos>>,
}

impl QueueState {
    pub fn new(class: WaitClass) -> Self {
        Self {
            queue: WaitQueue::new(class, StdLock::new(WaitList::new())),
            tokens: Cell::new(0),
            boost_until: Cell::new(None),
        }
    }
}

pub fn build_queues(scenario: &Scenario) -> Vec<QueueState> {
    scenario
        .queues
        .iter()
        .map(|spec| QueueState::new(spec.class))
        .collect()
}

pub struct ProcState {
    pub name: &'static str,
    pub share: Arc<FairShare<SimShareLock>>,
    /// The process's own reference to its address space. Dropped when the
    /// process concludes every one of its threads is gone — under the new
    /// protocol because they were all finalized, under the old one because a
    /// scan failed to find them.
    pub address_space: Option<StdArc<MockAddressSpace>>,
    pub templates: Vec<Script>,
    pub rt: bool,
    pub live: BTreeSet<TaskKey>,
    pub torn_down: bool,
}

/// A task's position in its script.
pub struct Program {
    pub process: usize,
    pub template: usize,
    pub pc: usize,
    pub iteration: usize,
    /// Remaining nanoseconds of the current `Run` op.
    pub run_left: u64,
}

/// A block that has done phase 1 and owes phase 2 (spec §8.1).
///
/// It is held *between* two steps, which is the whole point: the wait is
/// registered, the task is still running, and every other CPU in the system
/// can take a step before the blocking pass happens. That is the window the
/// kernel's lost wake lived in, and it exists only because the two halves are
/// two steps.
pub enum BlockPhase<'q> {
    /// The ticket is registered and uncommitted; the commit CAS belongs to the
    /// pass (spec §8.1, kernel since `8508b37`).
    Registered(WaitTicket<'q, SimMsg, SimWaitList>),
    /// The commit already ran at the call site (pre-`8508b37`): the word reads
    /// `Blocked` while the task is still `CpuSched.running`.
    Committed(CommittedTicket<SimMsg>),
}

/// How phase 2 of a block ended — the three ways `WaitTicket::commit` can
/// answer, as the workload driver has to see them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BlockEnd {
    Parked,
    /// A wake claimed the registration first: the task kept the CPU and its
    /// wait is satisfied.
    Woken,
    /// A retire landed inside the window; the task exited instead of parking.
    Killed,
}

/// A block in progress on one CPU.
pub struct Blocking<'q> {
    pub key: TaskKey,
    pub queue: usize,
    pub deadline: Option<Nanos>,
    pub phase: BlockPhase<'q>,
}

/// One step of the enabled-step relation (spec §10.2).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step {
    /// Advance the task running on this CPU by one op (or one run chunk).
    Exec(usize),
    /// Phase 2 of a block: the pass that commits the ticket and parks. A step
    /// of its own so that the interval between a wait's registration and its
    /// park is an interval other CPUs can act in.
    BlockPass(usize),
    /// An involuntary scheduler pass: IRQ exit with `need_resched`, or the
    /// idle loop.
    Pass(usize),
    DeliverIpi(usize),
    FireTimer(usize),
    /// A device completion interrupt (audio): wakes its queue's waiters with
    /// the IRQ-time boost, exactly as the ISR tail does.
    DeviceIrq(usize),
    /// Every CPU is halted: jump the clock to the earliest armed timer.
    Advance,
    /// OLD protocol only: an idle CPU pops a ready task out of a sibling's
    /// queue and carries it on its own stack.
    OldSteal {
        thief: usize,
        victim: usize,
    },
    /// OLD protocol only: the carried task lands in the thief's queue.
    OldInstall(usize),
}

pub struct Vm<'q> {
    pub scenario: Scenario,
    pub queues: &'q [QueueState],
    pub hw: SimHw,
    pub handles: SimHandles,
    pub frontier: Frontier,
    pub cpus: Vec<CpuSched<SimPayload>>,
    pub procs: Vec<ProcState>,
    pub programs: BTreeMap<TaskKey, Program>,
    /// Every task ever created, so a finalize-twice is detectable.
    pub spawned: BTreeSet<TaskKey>,
    pub live: BTreeSet<TaskKey>,
    pub shared: BTreeMap<TaskKey, Arc<TaskShared<SimMsg>>>,
    /// Registrations held across a block, exactly where a kernel blocking
    /// site holds them: on the waiting task's own stack.
    pub registrations: BTreeMap<TaskKey, Registration<'q, SimMsg, SimWaitList>>,
    /// Per CPU: a block that has registered and not yet parked.
    pub blocking: Vec<Option<Blocking<'q>>>,
    /// OLD protocol only: the unlocked transit slot.
    pub transit: Vec<Option<ReadyTask<SimPayload>>>,
    pub clock: Nanos,
    pub busy_ns: Vec<u64>,
    /// When each CPU's pending IPI must be taken.
    pub ipi_due: Vec<Option<Nanos>>,
    /// When each CPU last acquired an unserved `need_resched`. A CPU that
    /// owes a rescheduling pass takes it at IRQ exit, i.e. essentially at
    /// once; letting the explorer defer it while the clock ran would measure
    /// the explorer's freedom rather than the protocol's latency.
    pub resched_at: Vec<Option<Nanos>>,
    /// Busy-time stamp at which an RT task became ready while this CPU ran a
    /// normal one (invariant I4).
    pub rt_pending_since: Vec<Option<u64>>,
    pub next_irq: Vec<Nanos>,
    pub next_key: u64,
    /// Rotates the tie-break in spawn placement.
    pub next_spawn_cpu: usize,
    /// What `finalize()` handed back, for the accounting conservation check.
    pub finalized: Vec<(TaskKey, TaskAccounting)>,
    pub violations: Vec<String>,
    pub steps: usize,
    /// How many parks published `Blocked` and had it claimed before the park
    /// itself ran — spec §8.1's residual window, and the only thing that
    /// exercises `RunningTask::park`'s `WakeQueued` arm.
    pub pre_park_claims: u64,
    /// How many blocks ended in `Commit::Killed` — a retire that landed inside
    /// the registration window. Counted for the same reason as
    /// `pre_park_claims`: this driver has no kill check of its own any more, so
    /// a clean run is only evidence about the core's if the case occurred.
    pub killed_at_park: u64,
    /// Invariant I9's accumulator: per task, the lend counter it was last seen
    /// with and the running time charged to it since, while boosted. Reset when
    /// the counter moves, which is the only way to tell a fresh lend from
    /// [`toyos_sched::task::RtState::arm`]'s re-arm from outside the core.
    pub boosted_run: BTreeMap<TaskKey, (u32, u64)>,
}

impl<'q> Vm<'q> {
    pub fn new(scenario: Scenario, queues: &'q [QueueState]) -> Self {
        let n = scenario.cpus;
        let hw = SimHw::new(n);
        let mut handles = Vec::with_capacity(n);
        let mut cpus = Vec::with_capacity(n);
        for i in 0..n {
            let (tx, rx) = mailbox();
            handles.push(CpuHandle::new(CpuId(i as u32), tx));
            let mut cpu = CpuSched::new(CpuId(i as u32), rx, SimCtx::default());
            cpu.set_park_keeps_lapsed_lend(scenario.park == ParkShape::KeepLapsedLend);
            cpus.push(cpu);
        }
        let procs = scenario
            .procs
            .iter()
            .enumerate()
            .map(|(index, spec)| ProcState {
                name: spec.name,
                share: Arc::new(FairShare::new(StdLock::new(ShareState::NonRunnable {
                    lag: 0,
                }))),
                address_space: Some(StdArc::new(MockAddressSpace {
                    process: index as u32,
                })),
                templates: spec.templates.clone(),
                rt: spec.rt,
                live: BTreeSet::new(),
                torn_down: false,
            })
            .collect();
        let next_irq = scenario
            .irqs
            .iter()
            .map(|irq| Nanos::ZERO.after(irq.period_ns))
            .collect();

        let mut vm = Self {
            queues,
            hw,
            handles: SimHandles::new(handles),
            frontier: Frontier::new(),
            cpus,
            procs,
            programs: BTreeMap::new(),
            spawned: BTreeSet::new(),
            live: BTreeSet::new(),
            shared: BTreeMap::new(),
            registrations: BTreeMap::new(),
            blocking: (0..n).map(|_| None).collect(),
            transit: (0..n).map(|_| None).collect(),
            clock: Nanos::ZERO,
            busy_ns: vec![0; n],
            ipi_due: vec![None; n],
            resched_at: vec![None; n],
            rt_pending_since: vec![None; n],
            boosted_run: BTreeMap::new(),
            next_irq,
            next_key: 1,
            next_spawn_cpu: 0,
            finalized: Vec::new(),
            violations: Vec::new(),
            steps: 0,
            pre_park_claims: 0,
            killed_at_park: 0,
            scenario,
        };
        for (index, spec) in vm.scenario.procs.clone().iter().enumerate() {
            for &template in &spec.initial {
                vm.spawn(index, template);
            }
        }
        vm
    }

    pub fn violate(&mut self, what: impl Into<String>) {
        self.violations.push(what.into());
    }

    pub fn failed(&self) -> bool {
        !self.violations.is_empty() || self.hw.with(|s| !s.violations.is_empty())
    }

    /// Every violation seen so far, from both the VM's walks and the `Hw`
    /// callbacks (which cannot unwind out of a core call).
    pub fn all_violations(&self) -> Vec<String> {
        let mut all = self.violations.clone();
        all.extend(self.hw.with(|s| s.violations.clone()));
        all
    }

    // ---------------------------------------------------------------- spawn

    fn spawn(&mut self, process: usize, template: usize) {
        if self.live.len() >= self.scenario.max_tasks {
            return;
        }
        let key = TaskKey(self.next_key);
        self.next_key += 1;

        let address_space = self.procs[process]
            .address_space
            .clone()
            .expect("spawning into a process whose address space is gone");
        let share = self.procs[process].share.clone();
        let rt = RtState {
            permanent: self.procs[process].rt,
            inherited: None,
            lends: 0,
        };
        // Spawn placement: the least-loaded CPU from the published counters
        // (spec §9.4) — never a try_lock probe of a remote queue, which is
        // what used to misread contention as emptiness. Ties rotate, or every
        // task of a freshly booted system would land on cpu0 and the
        // scenarios would never see two CPUs at once.
        let base = self.next_spawn_cpu;
        let dst = (0..self.scenario.cpus)
            .map(|offset| (base + offset) % self.scenario.cpus)
            .min_by_key(|&c| self.handles.get(CpuId(c as u32)).load())
            .expect("at least one cpu");
        self.next_spawn_cpu = (dst + 1) % self.scenario.cpus;
        let builder = TaskBuilder {
            key,
            share,
            ctx: SimCtx { key: Some(key) },
            ext: SimPayload {
                key,
                process: process as u32,
                address_space,
            },
            rt,
        };
        let task = builder.build(CpuId(dst as u32), self.clock);
        self.shared.insert(key, task.shared().clone());
        self.hw.with(|s| {
            s.ctx_saved.insert(key, true);
        });
        let handle = self.handles.get(CpuId(dst as u32));
        if handle.post_owned(
            Msg::Adopt { task },
            Msg::adopt_node,
            Urgency::Normal,
            &SimPreempt,
        ) == Kick::Send
        {
            self.hw.kick(CpuId(dst as u32));
            self.arm_ipi(dst);
        }
        self.programs.insert(
            key,
            Program {
                process,
                template,
                pc: 0,
                iteration: 0,
                run_left: 0,
            },
        );
        self.procs[process].live.insert(key);
        self.spawned.insert(key);
        self.live.insert(key);
    }

    fn arm_ipi(&mut self, cpu: usize) {
        if self.ipi_due[cpu].is_none() {
            self.ipi_due[cpu] = Some(self.clock.after(IPI_LATENCY_NS));
        }
    }

    // ------------------------------------------------------- enabled steps

    pub fn enabled(&self) -> Vec<Step> {
        let mut steps = Vec::new();
        let state = self.hw.with(|s| {
            (
                s.halted.clone(),
                s.need_resched.clone(),
                s.armed.clone(),
                s.pending_ipi.clone(),
            )
        });
        let (halted, need_resched, armed, pending_ipi) = state;

        // Hardware does not let time pass with an interrupt overdue, and
        // neither does the model: while any CPU owes a delivery, no execution
        // step — the only thing that advances the clock — is enabled. Without
        // this the explorer could hold one CPU at its interrupt while another
        // ran for milliseconds, and invariant I4 would be measuring the
        // explorer's freedom rather than the protocol's latency.
        let delivery_owed = (0..self.scenario.cpus).any(|cpu| {
            (pending_ipi[cpu] > 0 && self.ipi_due[cpu].is_some_and(|at| at <= self.clock))
                || armed[cpu].is_some_and(|at| at <= self.clock)
                || (need_resched[cpu]
                    && self.resched_at[cpu].is_some_and(|at| at.after(RUN_CHUNK_NS) <= self.clock))
        });

        for cpu in 0..self.scenario.cpus {
            if pending_ipi[cpu] > 0 {
                steps.push(Step::DeliverIpi(cpu));
            }
            if armed[cpu].is_some_and(|at| at <= self.clock) {
                steps.push(Step::FireTimer(cpu));
            }
            if halted[cpu] {
                continue;
            }
            // Mid-block: the task has registered on a wait queue and owes the
            // pass that parks it, so it cannot run another op.
            //
            // Whether it can be handed an *involuntary* pass is the kernel's
            // preempt count, modelled rather than assumed. The interrupt still
            // arrives — `DeliverIpi` and `FireTimer` are enabled above and set
            // `need_resched` — but the registration holds preemption off
            // (`kernel/src/sched/driver.rs`'s `Ticket`), so the pass it asks
            // for waits for the commit. That is the whole of the kernel's
            // deferred-preemption model, and it is why the window has exactly
            // one legal exit. `WindowShape::Preemptible` is the kernel without
            // that guard, and is a negative gate.
            if self.blocking[cpu].is_some() {
                steps.push(Step::BlockPass(cpu));
                if self.scenario.window == WindowShape::Preemptible && need_resched[cpu] {
                    steps.push(Step::Pass(cpu));
                }
                continue;
            }
            if self.cpus[cpu].running().is_some() && !need_resched[cpu] && !delivery_owed {
                steps.push(Step::Exec(cpu));
            }
            if need_resched[cpu] || self.cpus[cpu].running().is_none() {
                steps.push(Step::Pass(cpu));
            }
        }

        // A device keeps interrupting for as long as there is a system to
        // interrupt. With nothing left alive there is nobody to wake, and an
        // endless stream of them would keep the run from ever quiescing.
        if !self.live.is_empty() {
            for index in 0..self.scenario.irqs.len() {
                if self.next_irq[index] <= self.clock {
                    steps.push(Step::DeviceIrq(index));
                }
            }
        }

        if self.scenario.protocol == Protocol::OldSteal {
            for thief in 0..self.scenario.cpus {
                if halted[thief] {
                    continue;
                }
                if self.transit[thief].is_some() {
                    steps.push(Step::OldInstall(thief));
                    continue;
                }
                if self.cpus[thief].running().is_some() || !self.cpus[thief].rq().is_empty() {
                    continue;
                }
                for victim in 0..self.scenario.cpus {
                    if victim != thief && self.cpus[victim].rq().fair_len() > 0 {
                        steps.push(Step::OldSteal { thief, victim });
                    }
                }
            }
        }

        if steps.is_empty() {
            if let Some(next) = self.next_deadline() {
                if next > self.clock {
                    steps.push(Step::Advance);
                }
            }
        }
        steps
    }

    fn next_deadline(&self) -> Option<Nanos> {
        let armed = self.hw.with(|s| s.armed.clone());
        let irqs = if self.live.is_empty() {
            Vec::new()
        } else {
            self.next_irq.clone()
        };
        armed.into_iter().flatten().chain(irqs).min()
    }

    // ----------------------------------------------------------- execution

    pub fn execute(&mut self, step: Step, choices: &mut ChoiceStream) {
        self.steps += 1;
        self.execute_inner(step, choices);
        let owed = self.hw.with(|s| s.need_resched.clone());
        for cpu in 0..self.scenario.cpus {
            match (owed[cpu], self.resched_at[cpu]) {
                (true, None) => self.resched_at[cpu] = Some(self.clock),
                (false, _) => self.resched_at[cpu] = None,
                (true, Some(_)) => {}
            }
        }
    }

    fn execute_inner(&mut self, step: Step, choices: &mut ChoiceStream) {
        match step {
            Step::Exec(cpu) => self.exec_op(cpu, choices),
            Step::BlockPass(cpu) => self.block_pass(cpu, choices),
            Step::Pass(cpu) => {
                self.run_pass(cpu, Dispose::None);
            }
            Step::DeliverIpi(cpu) => {
                self.hw.with(|s| {
                    s.pending_ipi[cpu] -= 1;
                    s.need_resched[cpu] = true;
                    s.halted[cpu] = false;
                });
                self.ipi_due[cpu] = None;
            }
            Step::FireTimer(cpu) => {
                self.hw.with(|s| {
                    s.armed[cpu] = None;
                    s.need_resched[cpu] = true;
                    s.halted[cpu] = false;
                });
            }
            Step::DeviceIrq(index) => self.device_irq(index),
            Step::Advance => {
                if let Some(next) = self.next_deadline() {
                    let delta = next.since(self.clock);
                    self.advance(delta);
                }
            }
            Step::OldSteal { thief, victim } => self.old_steal(thief, victim),
            Step::OldInstall(thief) => self.old_install(thief),
        }
    }

    /// Advance the clock. Every CPU with a task loaded is executing during the
    /// interval — the model's serialization is an ordering device, not a
    /// claim that only one CPU runs at a time.
    fn advance(&mut self, delta: u64) {
        if delta == 0 {
            return;
        }
        for cpu in 0..self.scenario.cpus {
            let Some(task) = self.cpus[cpu].running() else {
                continue;
            };
            self.busy_ns[cpu] += delta;
            let (key, rt) = (task.key(), task.rt());
            let entry = self.boosted_run.entry(key).or_insert((rt.lends, 0));
            if entry.0 != rt.lends {
                *entry = (rt.lends, 0);
            }
            if rt.inherited.is_some() {
                entry.1 += delta;
            }
        }
        self.clock = self.clock.after(delta);
        self.hw.with(|s| s.now = self.clock);
    }

    fn device_irq(&mut self, index: usize) {
        let spec = self.scenario.irqs[index];
        self.next_irq[index] = self.next_irq[index].after(spec.period_ns);
        let queues = self.queues;
        let queue = &queues[spec.queue];
        let waiters = queue.queue.len().max(1) as u32;
        queue.tokens.set(queue.tokens.get() + waiters);
        let cause = match spec.boost_ns {
            Some(ns) => WakeCause::boosted(WakeReason::Woken, self.clock.after(ns)),
            None => WakeCause::new(WakeReason::Woken),
        };
        let kicks_before = self.hw.with(|s| s.kicks);
        queue
            .queue
            .wake_all(cause, &self.handles, &self.hw, &SimPreempt);
        self.note_kicks(kicks_before);
    }

    /// Any kick issued by a wake path arms its target's interrupt deadline.
    fn note_kicks(&mut self, before: u64) {
        if self.hw.with(|s| s.kicks) == before {
            return;
        }
        for cpu in 0..self.scenario.cpus {
            if self.hw.with(|s| s.pending_ipi[cpu]) > 0 {
                self.arm_ipi(cpu);
            }
        }
    }

    // ------------------------------------------------------------ the pass

    /// Run one pass. The returned [`BlockEnd`] is only meaningful for
    /// [`Dispose::Commit`]; every other disposition reports `Parked`, which
    /// nobody reads.
    fn run_pass(&mut self, cpu: usize, dispose: Dispose<'q>) -> BlockEnd {
        let now = self.clock;
        let kicks_before = self.hw.with(|s| {
            s.need_resched[cpu] = false;
            s.halted[cpu] = false;
            s.kicks
        });
        self.hw.enter_pass(CpuId(cpu as u32), now);
        // Copied out before the borrow: the injection below runs while the
        // pass holds `CpuSched`, and these are the only fields it needs.
        let queues = self.queues;
        let mut injected = None;
        let (action, parked, end) = {
            let Vm {
                cpus,
                hw,
                handles,
                frontier,
                ..
            } = self;
            let env = Env {
                hw,
                cpus: handles,
                frontier,
                preempt: &SimPreempt,
                steal: true,
            };
            let pass = SchedPass::begin(&mut cpus[cpu], env, now);
            match dispose {
                Dispose::None => (pass.dispose_none().finish(), None, BlockEnd::Parked),
                Dispose::Yield => (pass.dispose_yield().finish(), None, BlockEnd::Parked),
                Dispose::Exit => (pass.dispose_exit().finish(), None, BlockEnd::Parked),
                Dispose::Block(ticket, deadline) => (
                    pass.dispose_block(ticket, deadline).finish(),
                    None,
                    BlockEnd::Parked,
                ),
                // Phase 2 inside the pass, after `begin`'s drain (spec §8.1).
                // Committing here puts every claim on one side of the drain or
                // the other: an earlier one finds `Committing` and posts no
                // message, so this CAS observes it; a later one's message
                // arrives behind the drain and the next pass finds the task
                // parked.
                Dispose::Commit(ticket, deadline, after) => match ticket.commit() {
                    Commit::Parked(committed, registration) => {
                        let key = committed.shared().key();
                        // The residual window the fix names and cannot close:
                        // a waker may claim the task in the instructions
                        // between the commit publishing `Blocked` and the park
                        // itself. Its `Msg::Wake` lands *behind* this pass's
                        // drain, so the next pass finds the task parked and
                        // delivers it — which is the entire reason
                        // `RunningTask::park` accepts `WakeQueued`. It is
                        // injected here rather than reached by a step boundary
                        // because `SchedPass` borrows `CpuSched` and cannot be
                        // held across one; without it that arm is dead code in
                        // every simulator run.
                        if let Some(hoisted) = after {
                            wake(
                                queues,
                                now,
                                handles,
                                hw,
                                hoisted.queue,
                                hoisted.all,
                                hoisted.boost,
                            );
                            injected = Some(hoisted.key);
                        }
                        (
                            pass.dispose_block(committed, deadline).finish(),
                            Some((key, registration)),
                            BlockEnd::Parked,
                        )
                    }
                    // Do not park, do not switch. The pass still runs to a
                    // disposition, because the quantum may have expired while
                    // the decision was being made.
                    Commit::AlreadyWoken => {
                        (pass.dispose_none().finish(), None, BlockEnd::Woken)
                    }
                    // A retire landed while the task was deciding to park.
                    // Parking is a safe point (spec §6.3, §7.6): the commit
                    // withdrew the registration, and this pass buries it.
                    Commit::Killed => {
                        (pass.dispose_exit().finish(), None, BlockEnd::Killed)
                    }
                },
            }
        };
        if let Some((key, registration)) = parked {
            self.registrations.insert(key, registration);
            // Did the injected wake claim *this* task before its park ran?
            // Counted rather than argued: the arm it exercises was dead code in
            // every run this simulator had ever made.
            if injected.is_some() && matches!(self.shared[&key].state(), TaskState::WakeQueued(_)) {
                self.pre_park_claims += 1;
            }
        }
        if let Some(key) = injected {
            self.programs.get_mut(&key).expect("live").pc += 1;
        }
        if end == BlockEnd::Killed {
            self.killed_at_park += 1;
        }
        self.apply(action);
        self.hw.leave_pass();
        self.note_kicks(kicks_before);
        end
    }

    #[allow(unsafe_code)] // `Hw::switch` is an unsafe fn; SimHw's body derefs nothing
    fn apply(&mut self, action: Action<SimPayload>) {
        match action {
            // SAFETY: the token came from `finish()`, which built it from
            // live Box-backed records; `SimHw::switch` only reads the keys.
            Action::Run(token) => unsafe { self.hw.switch(token) },
            Action::Resume => {}
            Action::Idle(token) => self.hw.idle_wait(token),
        }
    }

    // ----------------------------------------------------------- workload

    fn exec_op(&mut self, cpu: usize, choices: &mut ChoiceStream) {
        let Some(key) = self.cpus[cpu].running().map(|t| t.key()) else {
            return;
        };
        // A killed task dies at its next safe point (spec §7.6). This is that
        // point: the same place a real thread would notice on its way out of
        // a syscall.
        if self.shared[&key].kill_pending() {
            self.finish_task(cpu, key);
            return;
        }
        let Some(program) = self.programs.get(&key) else {
            self.finish_task(cpu, key);
            return;
        };
        let script = &self.procs[program.process].templates[program.template];
        let Some(&op) = script.ops.get(program.pc) else {
            let iteration = program.iteration + 1;
            let repeat = script.repeat;
            let program = self.programs.get_mut(&key).expect("checked above");
            if iteration < repeat {
                program.pc = 0;
                program.iteration = iteration;
            } else {
                self.finish_task(cpu, key);
            }
            return;
        };

        match op {
            Op::Run(ns) => {
                let program = self.programs.get_mut(&key).expect("checked above");
                if program.run_left == 0 {
                    program.run_left = ns;
                }
                let chunk = program.run_left.min(RUN_CHUNK_NS);
                program.run_left -= chunk;
                if program.run_left == 0 {
                    program.pc += 1;
                }
                self.advance(chunk);
            }
            Op::KernelSection(ns) => {
                self.advance(ns);
                self.programs.get_mut(&key).expect("live").pc += 1;
            }
            Op::Yield => {
                self.programs.get_mut(&key).expect("live").pc += 1;
                self.run_pass(cpu, Dispose::Yield);
            }
            Op::SetRt => {
                self.cpus[cpu].set_current_rt(true);
                self.programs.get_mut(&key).expect("live").pc += 1;
            }
            Op::Spawn { template } => {
                let process = self.programs[&key].process;
                self.spawn(process, template);
                self.programs.get_mut(&key).expect("live").pc += 1;
            }
            Op::Wake { queue, all, boost } => {
                self.do_wake(queue, all, boost);
                self.programs.get_mut(&key).expect("live").pc += 1;
            }
            Op::Block { queue, deadline } => self.do_block(cpu, key, queue, deadline, choices),
            Op::Teardown => {
                self.teardown(key);
                self.finish_task(cpu, key);
            }
            Op::Exit => self.finish_task(cpu, key),
        }
    }

    fn do_wake(&mut self, queue: usize, all: bool, boost: Option<u64>) {
        let before = self.hw.with(|s| s.kicks);
        wake(
            self.queues,
            self.clock,
            &self.handles,
            &self.hw,
            queue,
            all,
            boost,
        );
        self.note_kicks(before);
    }

    /// The uniform blocking shape of spec §8.1, run by the task itself.
    fn do_block(
        &mut self,
        cpu: usize,
        key: TaskKey,
        queue: usize,
        deadline: Option<u64>,
        choices: &mut ChoiceStream,
    ) {
        // Resuming from a previous park. Clearing the registration first is
        // what keeps a timed-out waiter from leaving a node behind for the
        // next `wake_one` to waste itself on.
        //
        // One park completes one `Block`, whichever cause ended it — the
        // kernel's `block_on` returns `Woken` or `Timeout` and its caller
        // moves on either way. Retrying the same block on a timeout would be
        // a waiter that can never give up, which is a workload that never
        // terminates rather than a protocol under test.
        if let Some(registration) = self.registrations.remove(&key) {
            registration.finish();
            let q = &self.queues[queue];
            if q.tokens.get() > 0 {
                q.tokens.set(q.tokens.get() - 1);
            }
            self.programs.get_mut(&key).expect("live").pc += 1;
            return;
        }
        // Copy the arena reference out of `self` first: the ticket and the
        // registration borrow the queue for as long as the arena lives, not
        // for as long as this `&mut self` does.
        let queues = self.queues;
        let q = &queues[queue];
        if q.tokens.get() > 0 {
            q.tokens.set(q.tokens.get() - 1);
            self.take_pending_boost(cpu, queue);
            self.programs.get_mut(&key).expect("live").pc += 1;
            return;
        }

        let ticket = {
            let current = self.cpus[cpu]
                .current_task()
                .expect("blocking without a running task");
            q.queue.prepare_wait(&current)
        };

        // The registration is live and the task has not parked yet: this is
        // the window every one of the five lost-wake bugs lived in. Letting
        // the explorer put another CPU's wake *here* is what makes those
        // windows reachable rather than argued about (spec §10.2).
        if choices.choose(2) == 1 {
            self.interfere(cpu, queue);
        }

        // The re-check, at the call site where the kernel has it: register,
        // re-check, park.
        let q = &queues[queue];
        if q.tokens.get() > 0 {
            match ticket.cancel() {
                Cancelled::Clean => {
                    // Retry the same op; the token is taken on the next pass
                    // through this function.
                }
                Cancelled::AlreadyWoken => {
                    q.tokens.set(q.tokens.get() - 1);
                    self.programs.get_mut(&key).expect("live").pc += 1;
                }
            }
            return;
        }

        let deadline = deadline.map(|ns| self.clock.after(ns));
        let phase = match self.scenario.block {
            BlockShape::CommitInPass => BlockPhase::Registered(ticket),
            BlockShape::CommitAtCallSite | BlockShape::CommitAtCallSiteFused => {
                match ticket.commit() {
                    Commit::Parked(committed, registration) => {
                        self.registrations.insert(key, registration);
                        BlockPhase::Committed(committed)
                    }
                    // A wake landed between registration and commit: do not
                    // park, do not switch. The condition is satisfied.
                    Commit::AlreadyWoken => {
                        let q = &queues[queue];
                        if q.tokens.get() > 0 {
                            q.tokens.set(q.tokens.get() - 1);
                        }
                        self.programs.get_mut(&key).expect("live").pc += 1;
                        return;
                    }
                    // A retire beat the commit. Committing at the call site
                    // does not change what that means — the thread dies
                    // instead of parking — only where the pass that buries it
                    // is entered from.
                    Commit::Killed => {
                        self.finish_task(cpu, key);
                        return;
                    }
                }
            }
        };
        self.blocking[cpu] = Some(Blocking {
            key,
            queue,
            deadline,
            phase,
        });
        // The fused shape is the simulator's own pre-split behaviour: the pass
        // runs in the *same* step, so no other CPU can act between the two
        // halves and the window is outside the step relation entirely.
        if self.scenario.block == BlockShape::CommitAtCallSiteFused {
            self.block_pass(cpu, choices);
        }
    }

    /// Phase 2 of the wait handshake, as a step of its own.
    ///
    /// Splitting it out is the whole point: the kernel takes two steps here —
    /// the call site that registers and re-checks, and the pass that drains,
    /// commits and parks — and a remote CPU is free to claim the waiter
    /// between them. Fusing them, as this model used to, put that interval
    /// outside the step relation, which is why the simulator certified a
    /// protocol whose lost wake it could not execute (commit `8508b37`).
    ///
    /// The one interval this step boundary opens up and does *not* offer a
    /// pass into is the kernel's preempt-off registration window; see
    /// `enabled`, which models the guard rather than looking away from what
    /// happens without it.
    ///
    /// There is no kill check here. A `Retire` that lands between the two
    /// halves is honoured by `WaitTicket::commit`, in the core, where both
    /// this driver and the kernel's get it — which is where it belongs, since
    /// a driver that forgot it would leave the task parked with nothing left
    /// to reap it.
    fn block_pass(&mut self, cpu: usize, choices: &mut ChoiceStream) {
        let Blocking {
            key,
            queue,
            deadline,
            phase,
        } = self.blocking[cpu]
            .take()
            .expect("a block pass with no block in progress");

        let end = match phase {
            BlockPhase::Registered(ticket) => {
                // The commit and the park are one step here — a `SchedPass`
                // borrows `CpuSched` and cannot be held across a step boundary
                // — so the interval between them is reached by injection
                // rather than by interleaving. `run_pass` explains what lives
                // there and why the arm it exercises would otherwise be dead.
                let after = (choices.choose(2) == 1)
                    .then(|| self.hoist_wake(cpu, queue))
                    .flatten();
                self.run_pass(cpu, Dispose::Commit(ticket, deadline, after))
            }
            // A ticket committed at the call site has already published
            // `Blocked`; there is no route back to `Running`, which is one more
            // thing wrong with committing there.
            BlockPhase::Committed(ticket) => {
                self.run_pass(cpu, Dispose::Block(ticket, deadline))
            }
        };
        match end {
            BlockEnd::Parked => {}
            // The task is dead. `reap_released` takes its program and its
            // bookkeeping; there is no registration to finish, because the
            // commit withdrew it.
            BlockEnd::Killed => {}
            // Phase 2 declined to park: the waker that claimed the ticket left
            // a token behind, and the script moves on.
            BlockEnd::Woken => {
                let q = &self.queues[queue];
                if q.tokens.get() > 0 {
                    q.tokens.set(q.tokens.get() - 1);
                }
                self.programs.get_mut(&key).expect("live").pc += 1;
            }
        }
    }

    /// The consume-side half of priority inheritance: a client that was
    /// already running when its producer signalled takes the window here
    /// rather than through a wake cause it never received (spec §8.5).
    fn take_pending_boost(&mut self, cpu: usize, queue: usize) {
        if let Some(until) = self.queues[queue].boost_until.get() {
            if until > self.clock {
                self.cpus[cpu].boost_current(until);
            } else {
                self.queues[queue].boost_until.set(None);
            }
        }
    }

    /// Find a task on another CPU whose very next op is a wake on `queue` —
    /// one that was going to happen anyway, so issuing it early perturbs the
    /// *timing* of the workload and not its token accounting.
    fn hoist_wake(&self, blocking_cpu: usize, queue: usize) -> Option<HoistedWake> {
        for cpu in 0..self.scenario.cpus {
            if cpu == blocking_cpu {
                continue;
            }
            let Some(key) = self.cpus[cpu].running().map(|t| t.key()) else {
                continue;
            };
            let Some(program) = self.programs.get(&key) else {
                continue;
            };
            let script = &self.procs[program.process].templates[program.template];
            if let Some(Op::Wake {
                queue: q,
                all,
                boost,
            }) = script.ops.get(program.pc).copied()
            {
                if q == queue {
                    return Some(HoistedWake {
                        key,
                        queue,
                        all,
                        boost,
                    });
                }
            }
        }
        None
    }

    /// One interfering wake from another CPU, issued in the window between a
    /// wait's registration and its commit. Bounded and non-blocking, so it
    /// cannot recurse into another registration window.
    fn interfere(&mut self, blocking_cpu: usize, queue: usize) {
        let Some(hoisted) = self.hoist_wake(blocking_cpu, queue) else {
            return;
        };
        self.do_wake(hoisted.queue, hoisted.all, hoisted.boost);
        self.programs.get_mut(&hoisted.key).expect("live").pc += 1;
    }

    fn finish_task(&mut self, cpu: usize, key: TaskKey) {
        if let Some(registration) = self.registrations.remove(&key) {
            registration.finish();
        }
        self.programs.remove(&key);
        self.run_pass(cpu, Dispose::Exit);
    }

    // ------------------------------------------------------------ teardown

    /// Process teardown: every other thread of this process must go.
    fn teardown(&mut self, by: TaskKey) {
        let process = self.programs[&by].process;
        self.procs[process].torn_down = true;
        let siblings: Vec<TaskKey> = self.procs[process]
            .live
            .iter()
            .copied()
            .filter(|&k| k != by)
            .collect();
        match self.scenario.protocol {
            Protocol::New => {
                for key in siblings {
                    let shared = self.shared[&key].clone();
                    let before = self.hw.with(|s| s.kicks);
                    retire::begin(&shared).post(&self.handles, &self.hw, &SimPreempt);
                    self.note_kicks(before);
                }
            }
            Protocol::OldSteal => self.old_teardown(process, siblings),
        }
    }

    /// The OLD decision procedure (`retire_task` + `scan_remove`): mark the
    /// task killed, then walk every container. "Not found anywhere" was taken
    /// as proof the task was gone — and a task carried on an idle CPU's stack
    /// mid-steal is in no container. The teardown then frees the address
    /// space, believing itself the last owner.
    fn old_teardown(&mut self, process: usize, siblings: Vec<TaskKey>) {
        let mut absent = Vec::new();
        for key in siblings {
            let shared = self.shared[&key].clone();
            shared.mark_kill();
            if self.scan_containers(key) {
                let before = self.hw.with(|s| s.kicks);
                retire::begin(&shared).post(&self.handles, &self.hw, &SimPreempt);
                self.note_kicks(before);
            } else {
                absent.push(key);
            }
        }
        if absent.is_empty() {
            return;
        }
        // Proof of absence, drawn. Every task it covers is declared gone, and
        // the address space is released on that basis.
        for key in &absent {
            self.procs[process].live.remove(key);
        }
        self.free_address_space(process);
    }

    fn scan_containers(&self, key: TaskKey) -> bool {
        (0..self.scenario.cpus)
            .any(|cpu| toyos_sched::invariants::residents(&self.cpus[cpu]).any(|(k, _)| k == key))
    }

    /// Release the process's own reference, asserting what the kernel's
    /// teardown assumes when it drops the last `Arc`: that nothing else still
    /// points at this address space. Invariant I8 — the crash.md detector.
    fn free_address_space(&mut self, process: usize) {
        let Some(space) = self.procs[process].address_space.take() else {
            return;
        };
        let count = StdArc::strong_count(&space);
        if count != 1 {
            let name = self.procs[process].name;
            self.violate(format!(
                "I8: {name} freed its address space while {} live task(s) still reference it",
                count - 1,
            ));
        }
        drop(space);
    }

    /// Called from the explorer after every step: a process whose threads are
    /// all finalized may let its address space go.
    pub fn collect_dead_processes(&mut self) {
        for process in 0..self.procs.len() {
            if !self.procs[process].torn_down || self.procs[process].address_space.is_none() {
                continue;
            }
            if self.procs[process].live.is_empty() {
                self.free_address_space(process);
            }
        }
    }

    // ------------------------------------------------------ old steal port

    fn old_steal(&mut self, thief: usize, victim: usize) {
        if let Some(task) = self.cpus[victim].steal_ready() {
            self.transit[thief] = Some(task);
        }
    }

    fn old_install(&mut self, thief: usize) {
        if let Some(task) = self.transit[thief].take() {
            self.cpus[thief].install_stolen(task);
        }
    }

    // ------------------------------------------------------- observations

    /// Reconcile the VM's own bookkeeping with what the core released.
    pub fn reap_released(&mut self) {
        let released = self.hw.with(|s| std::mem::take(&mut s.released));
        for (key, acct) in released {
            if !self.live.remove(&key) {
                self.violate(format!("I10: {key:?} was finalized twice"));
            }
            for process in &mut self.procs {
                process.live.remove(&key);
            }
            self.programs.remove(&key);
            // A task retired while parked never runs again, so nobody else
            // will clear its registration. The kernel's equivalent is the
            // reap path dequeuing the waiter it just killed.
            if let Some(registration) = self.registrations.remove(&key) {
                registration.finish();
            }
            self.finalized.push((key, acct));
        }
    }

    pub fn process_of(&self, key: TaskKey) -> Option<usize> {
        self.procs.iter().position(|p| p.live.contains(&key))
    }

    /// The scenario's preempt-off budget, cached for the per-step checks.
    pub fn max_kernel_section(&self) -> u64 {
        self.scenario.max_kernel_section()
    }
}

/// Make a queue's condition true and wake its waiters.
///
/// A free function rather than a method because one caller — the injection
/// that reaches the window *inside* the blocking pass — runs while `CpuSched`
/// is mutably borrowed by that pass and can only hand over the fields a wake
/// actually needs.
fn wake(
    queues: &[QueueState],
    now: Nanos,
    handles: &SimHandles,
    hw: &SimHw,
    queue: usize,
    all: bool,
    boost: Option<u64>,
) {
    let q = &queues[queue];
    let tokens = if all { q.queue.len().max(1) as u32 } else { 1 };
    q.tokens.set(q.tokens.get() + tokens);
    let cause = match boost {
        Some(ns) => {
            let until = now.after(ns);
            q.boost_until.set(Some(until));
            WakeCause::boosted(WakeReason::Woken, until)
        }
        None => WakeCause::new(WakeReason::Woken),
    };
    if all {
        q.queue.wake_all(cause, handles, hw, &SimPreempt);
    } else {
        q.queue.wake_one(cause, handles, hw, &SimPreempt);
    }
}

/// A wake some other CPU's task was about to perform, lifted out of its script
/// so it can be issued at a point the ordinary `Exec` step cannot reach.
#[derive(Clone, Copy)]
pub struct HoistedWake {
    /// Whose script it came from; its program counter advances once it runs.
    pub key: TaskKey,
    pub queue: usize,
    pub all: bool,
    pub boost: Option<u64>,
}

/// How a pass is disposed. One helper covers all of them, so the borrow dance
/// that hands `CpuSched` to the pass exists once.
pub enum Dispose<'q> {
    None,
    Yield,
    Exit,
    /// Park with a ticket that was committed before the pass was entered.
    Block(CommittedTicket<SimMsg>, Option<Nanos>),
    /// Commit *inside* the pass, after its drain, and park with the result —
    /// spec §8.1's phase 2. The optional wake is issued between the commit and
    /// the park; see [`Vm::run_pass`].
    Commit(
        WaitTicket<'q, SimMsg, SimWaitList>,
        Option<Nanos>,
        Option<HoistedWake>,
    ),
}
