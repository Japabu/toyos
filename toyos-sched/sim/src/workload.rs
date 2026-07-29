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
}
