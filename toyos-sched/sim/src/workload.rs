//! The workload script DSL — spec §10.2's `Run | Block | Wake | Spawn | Exit
//! | FutexOp | IrqAt | KernelSection(ns)`.
//!
//! A scenario is *data*: CPUs, wait queues, processes and their thread
//! scripts. Everything a scenario can express is something the kernel's own
//! blocking sites do, so a scenario that passes is a statement about the
//! protocol rather than about the harness.
//!
//! Futexes get no opcode of their own: a futex bucket *is* a `WaitQueue`
//! (spec §8.6), so a futex storm is `Block`/`Wake` on a queue whose class is
//! `Futex`. Giving it a second opcode would be modelling a second wake path
//! — the very thing §8.2 removes.

use toyos_sched::queue::FairOrder;
use toyos_sched::task::WaitClass;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    /// Consume CPU time. The VM splits it into chunks so that a quantum can
    /// expire in the middle of one.
    Run(u64),
    /// Preempt-disabled kernel work: consumed atomically, so it bounds RT
    /// wake latency exactly as a real preempt-off section does (invariant
    /// I4's `max KernelSection` term).
    KernelSection(u64),
    /// The uniform blocking shape: try, register, re-check, park.
    Block {
        queue: usize,
        deadline: Option<u64>,
    },
    /// Make the queue's condition true and wake a waiter (or all of them).
    Wake {
        queue: usize,
        all: bool,
        /// Lend the woken task RT for this long — soundd signalling its
        /// clients (spec §8.5).
        boost: Option<u64>,
    },
    Yield,
    /// Start another thread of this process from the process's template list.
    Spawn {
        template: usize,
    },
    /// Become RT permanently (the privilege-gated syscall).
    SetRt,
    /// Retire every *other* thread of this process, then exit: process
    /// teardown, the shape crash.md died in.
    Teardown,
    Exit,
}

#[derive(Clone, Debug)]
pub struct Script {
    pub ops: Vec<Op>,
    /// How many times to run `ops` before falling off the end (which exits).
    pub repeat: usize,
}

impl Script {
    pub fn new(ops: Vec<Op>) -> Self {
        Self { ops, repeat: 1 }
    }

    pub fn looping(ops: Vec<Op>, repeat: usize) -> Self {
        Self { ops, repeat }
    }
}

#[derive(Clone, Debug)]
pub struct ProcSpec {
    pub name: &'static str,
    /// Threads started with the process.
    pub initial: Vec<usize>,
    /// Scripts a `Spawn` op can instantiate, indexed by `template`.
    pub templates: Vec<Script>,
    /// Threads of this process start out real-time (soundd).
    pub rt: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct QueueSpec {
    pub class: WaitClass,
}

/// A periodic device interrupt — the audio card's completion IRQ. Delivery is
/// a step the explorer schedules, so its position relative to everything else
/// is part of the search space.
#[derive(Clone, Copy, Debug)]
pub struct IrqSpec {
    pub period_ns: u64,
    pub queue: usize,
    pub boost_ns: Option<u64>,
}

/// Where phase 2 of the §8.1 handshake — the `Committing(gen) → Blocked` CAS —
/// runs relative to the blocking pass.
///
/// This is a scenario dimension rather than a constant because the kernel has
/// had two of these answers, and the difference between them was a real lost
/// wake (commit `8508b37`). The VM makes the *step boundary* the thing that
/// moves: a remote CPU can act between two steps and cannot act inside one, so
/// which side of the boundary the commit falls on is exactly what decides
/// whether the window is reachable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BlockShape {
    /// Spec §8.1, and the kernel since `8508b37`: the commit runs inside the
    /// pass, after its mailbox drain. Every claim then lands on one side of
    /// the drain or the other.
    CommitInPass,
    /// The kernel before `8508b37`: the commit runs at the call site, so the
    /// task's word reads `Blocked` while it is still the running task and its
    /// own CPU has not yet drained. See `scenarios::old_commit_before_pass`.
    CommitAtCallSite,
    /// `CommitAtCallSite` with the call site and the pass **fused into one
    /// step** — the *simulator's* own shape until the split. Nothing can
    /// interleave, so the window is outside the step relation and the bug is
    /// invisible. It exists so that the harness's blind spot is a test rather
    /// than a comment (`blind_spot_needed_the_step_split`).
    CommitAtCallSiteFused,
}

/// Whether an *involuntary* scheduler pass can land inside the registration
/// window — between phase 1 of the §8.1 handshake and phase 2.
///
/// The window is two steps, so an interrupt can arrive in the middle of it;
/// `DeliverIpi` and `FireTimer` are enabled there and set `need_resched`. What
/// this decides is whether the pass that request asks for may *run* before the
/// commit, which in the kernel is decided by the preempt count and by nothing
/// else. It is a scenario dimension rather than a constant for the same reason
/// [`BlockShape`] is: the kernel has had both answers, and the difference
/// between them was a live panic.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WindowShape {
    /// The kernel's `sched::driver::Ticket` holds the preempt count raised for
    /// the whole window, so the request is deferred to the blocking pass.
    PreemptOff,
    /// No guard: the `preempt::enable` at the tail of any lock the re-check
    /// takes drops the count to zero and runs a pass on a task whose word
    /// reads `Committing`. See `scenarios::old_preemptible_window`.
    Preemptible,
}

/// What a `park` does to a borrowed RT window.
///
/// A scenario dimension rather than a constant for the same reason
/// [`BlockShape`] and [`WindowShape`] are: the kernel has had both answers, and
/// the difference between them is invariant I9's whole content.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ParkShape {
    /// Spec §8.5, and the kernel since the park was made unconditional: a block
    /// ends the hold outright, whatever the clock says.
    ReleaseLend,
    /// Commit `9c2fc4d`: clear only `if now >= until`, so a lend blocked on
    /// before it ran out survives the block and `RtState::arm` re-arms it at the
    /// next dispatch. One lend then buys unbounded RT.
    /// See `scenarios::old_park_kept_the_lend`.
    KeepLapsedLend,
}

/// What the balance path does with a ready task whose kill bit is already set.
///
/// A scenario dimension rather than a constant for the same reason
/// [`BlockShape`] and [`ParkShape`] are: the kernel has had both answers, and
/// the difference between them was a live panic on the owner's T14 —
/// `retire_task: task not released after 1s: InTransit(CpuId(1))`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MigrateShape {
    /// Spec §7.6's promptness carried into the balance path: a killed task is
    /// kept by the CPU that holds it and dispatched there, never handed on.
    /// The variant keeps the name it was minted with, and the name is now the
    /// *old* mechanism — §7.2 replaced the reap with a dispatch and the
    /// promptness argument is unchanged by that.
    ReapTheCorpse,
    /// The balance path before it read the kill bit: a killed ready task is
    /// migrated like any other, and its unwind then waits on an
    /// `Urgency::Normal` adopt reaching a CPU that owes it nothing sooner than
    /// its next voluntary pass. See `scenarios::old_migrate_kept_the_corpse`.
    KeepTheCorpse,
}

/// How the pick weighs a ready real-time task against a corpse waiting to
/// unwind.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AgeShape {
    /// What ships: the RT band goes first until the head of the dying list has
    /// waited `DYING_AGE_NS`, and then that corpse takes one `DYING_CHUNK_NS`
    /// ahead of it. Bounded in both directions.
    BoundedDeferral,
    /// The shape this branch shipped between the two fixes: `pick` asks only
    /// `rq.has_rt()`, so a permanently-RT thread that never parks holds the
    /// dying list closed for ever and `scheduler::retire_task`'s tripwire
    /// panics the kernel. See `scenarios::old_rt_starved_the_corpse`.
    RtOutranksEveryCorpse,
}

/// What a fair share is a share *of* — spec §9.1's "all threads of one process
/// share a vruntime".
///
/// A scenario dimension rather than a constant for the same reason
/// [`BlockShape`] and [`ParkShape`] are, except that the other answer is one
/// the design *rejected* rather than one it shipped: §13.9 turns down
/// per-thread weight-division fairness, and per-thread vruntime is that policy
/// in its simplest form. Invariant I5 has to be able to tell the two apart, or
/// it is not measuring the policy the spec states.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShareShape {
    /// Spec §9.1: one [`toyos_sched::fair::FairShare`] per process, reached
    /// through every thread of it.
    PerProcess,
    /// One share per thread, so a process buys CPU by forking. See
    /// `scenarios::fair_share_per_thread`.
    PerThread,
}

/// How much vruntime a running task's share is charged for the time it runs.
///
/// The honest answer is "the time it ran", and it is the core that computes it
/// (`SchedPass::begin`). This dimension lets the VM charge a named process a
/// second time on top, which is the shape of a charge applied at two
/// transitions instead of one — and the shape invariant I5 must notice,
/// because a share whose vruntime outruns its service is a share being
/// throttled for work it never did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChargeShape {
    Honest,
    /// Charge this process's share twice for every nanosecond it runs. See
    /// `scenarios::fair_double_charge`.
    Double { process: &'static str },
}

/// Where a spawn is placed.
///
/// A scenario dimension rather than a constant because the shipped answer is a
/// *policy*, and it is the one that decides which machines the rest of the
/// scheduler is ever measured on: least-loaded-with-rotation spreads every
/// burst by construction, so under it a machine whose runnable threads all sit
/// on one CPU is not expressible from a workload at all — and that machine is
/// exactly what the pull half of §7.7 exists for. Both answers below are the
/// kernel's own code either way; what this decides is which of the two numbers
/// `Vm::spawn` places by.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PlacementShape {
    /// Spec §9.4, and `kernel::sched::driver`'s `placement`: the least-loaded
    /// CPU from the published counters, ties rotating so a freshly booted
    /// system does not put everything on cpu0.
    LeastLoadedRotating,
    /// Every spawn onto one named CPU, whatever the counters say.
    ///
    /// **Not a policy this kernel has ever had, and that is the point.** It is
    /// the adversary: the lopsided machine no legal placement produces and no
    /// workload can otherwise stage, so that what the balance path does with
    /// one is a measurement rather than a belief. See
    /// `scenarios::lopsided_placement`.
    AllOn(usize),
}

/// Whether the pull half of the balance path runs.
///
/// A scenario dimension for [`PlacementShape`]'s reason and with the same two
/// roles: the shipped answer, and the control that says what the measurement
/// would be without it. `kernel::sched::driver::env` sets `steal: true` and
/// never clears it, so [`BalanceShape::None`] is a kernel this tree has not
/// shipped — `toyos-sched/loom`'s `loom_retire` already builds an `Env` with
/// `steal: false` for the same reason, to state what the rest of the protocol
/// does without it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BalanceShape {
    /// Spec §7.7 and §9.4's pull half: an idle pass probes the CPU with the
    /// most surplus, and a loaded pass answers a probe out of `pop_surplus`.
    Pull,
    /// No probe and no answer. A task woken or placed onto a busy CPU waits
    /// there until that CPU's own queue reaches it.
    None,
}

/// Which teardown/balance algorithm the VM drives the core with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Protocol {
    /// This spec's protocol: balance by `StealRequest` message, retire by
    /// message to the home CPU named in the state word.
    New,
    /// The OLD kernel's: an idle CPU pops a ready task straight out of a
    /// sibling's queue and carries it, unlocked, on its own stack; a retirer
    /// scans every container and treats "not found" as proof of absence.
    /// See `scenarios::old_steal_port`.
    OldSteal,
}

#[derive(Clone, Debug)]
pub struct Scenario {
    pub name: &'static str,
    pub cpus: usize,
    pub queues: Vec<QueueSpec>,
    pub procs: Vec<ProcSpec>,
    pub irqs: Vec<IrqSpec>,
    pub protocol: Protocol,
    pub block: BlockShape,
    pub window: WindowShape,
    pub park: ParkShape,
    pub migrate: MigrateShape,
    pub age: AgeShape,
    pub share: ShareShape,
    pub charge: ChargeShape,
    pub placement: PlacementShape,
    pub balance: BalanceShape,
    /// How the fair band picks between two ready threads of one share. A
    /// scenario dimension for the same reason [`ShareShape`] is, and the type is
    /// the core's own rather than a parallel copy of it: the broken orderings
    /// live in `queue.rs` behind `protocol-port`, so the kernel cannot reach
    /// them and the simulator drives the real code either way.
    pub order: FairOrder,
    /// What one scheduler pass is modelled to cost. Zero everywhere but
    /// `scenarios::overlong_pass`; see [`crate::hw_impl::SimHwState`].
    pub pass_cost_ns: u64,
    /// Invariant I5's *recorded* ceiling, where the shipped scheduler does not
    /// meet the derived one — `scenarios::FAIRNESS_SAMPLE`. Zero means the
    /// derived bound is the ceiling, which is the only honest default: an
    /// allowance is a statement that a measurement was taken, and a scenario
    /// nobody has measured has no allowance to offer.
    pub fair_allowance_ns: u64,
    /// Invariant I13's recorded ceiling, in the same role and with the same
    /// default as `fair_allowance_ns`: zero unless somebody has measured this
    /// scenario and found the shipped scheduler past the derived per-thread
    /// bound.
    pub thread_allowance_ns: u64,
    /// Safety net: a run that has not quiesced by here is reported as a
    /// non-termination failure rather than looping forever.
    pub max_steps: usize,
    /// Cap on concurrently live tasks, so a spawn storm stays bounded.
    pub max_tasks: usize,
}

impl Scenario {
    /// The longest preempt-off section any thread of this scenario runs. It
    /// is a *term* of invariant I4's RT latency bound, not an excuse for it:
    /// making the budget visible is what stops "the sim cannot see kernel
    /// critical sections" from being a blind spot (spec §10.2).
    pub fn max_kernel_section(&self) -> u64 {
        self.procs
            .iter()
            .flat_map(|p| p.templates.iter())
            .flat_map(|s| s.ops.iter())
            .filter_map(|op| match op {
                Op::KernelSection(ns) => Some(*ns),
                _ => None,
            })
            .max()
            .unwrap_or(0)
    }
}

impl Scenario {
    pub fn with_protocol(mut self, protocol: Protocol) -> Self {
        self.protocol = protocol;
        self
    }

    pub fn with_cpus(mut self, cpus: usize) -> Self {
        self.cpus = cpus;
        self
    }

    pub fn with_block(mut self, block: BlockShape) -> Self {
        self.block = block;
        self
    }

    pub fn with_window(mut self, window: WindowShape) -> Self {
        self.window = window;
        self
    }

    pub fn with_park(mut self, park: ParkShape) -> Self {
        self.park = park;
        self
    }

    pub fn with_migrate(mut self, migrate: MigrateShape) -> Self {
        self.migrate = migrate;
        self
    }

    pub fn with_age(mut self, age: AgeShape) -> Self {
        self.age = age;
        self
    }

    pub fn with_share(mut self, share: ShareShape) -> Self {
        self.share = share;
        self
    }

    pub fn with_charge(mut self, charge: ChargeShape) -> Self {
        self.charge = charge;
        self
    }

    /// **Checked here rather than at the placement site**, which runs once per
    /// spawn: a CPU index outside the machine is a scenario that was written
    /// wrong, and a panic at the first spawn of a sweep would name the VM
    /// instead of the scenario that asked for it.
    pub fn with_placement(mut self, placement: PlacementShape) -> Self {
        if let PlacementShape::AllOn(cpu) = placement {
            assert!(
                cpu < self.cpus,
                "{}: placement names cpu{cpu} on a {}-cpu machine",
                self.name,
                self.cpus,
            );
        }
        self.placement = placement;
        self
    }

    pub fn with_balance(mut self, balance: BalanceShape) -> Self {
        self.balance = balance;
        self
    }

    pub fn with_order(mut self, order: FairOrder) -> Self {
        self.order = order;
        self
    }

    pub fn with_pass_cost(mut self, ns: u64) -> Self {
        self.pass_cost_ns = ns;
        self
    }

    pub fn with_fair_allowance(mut self, ns: u64) -> Self {
        self.fair_allowance_ns = ns;
        self
    }

    /// The index of a process by name, for the dimensions that name one.
    pub fn process_index(&self, name: &str) -> Option<usize> {
        self.procs.iter().position(|p| p.name == name)
    }
}
