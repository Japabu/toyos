//! The scenario library — spec §11's Stage 4 row.
//!
//! Each scenario is a shape the kernel actually has, written as data. They are
//! deliberately small: the search space is the *interleaving*, not the
//! workload, and a scenario that takes ten thousand steps to quiesce buys one
//! schedule per second instead of a thousand.

use toyos_sched::task::WaitClass;

use crate::workload::{
    BlockShape, ChargeShape, IrqSpec, Op, ParkShape, ProcSpec, Protocol, QueueSpec, Scenario,
    Script, ShareShape, WindowShape,
};

const MS: u64 = 1_000_000;

fn queue(class: WaitClass) -> QueueSpec {
    QueueSpec { class }
}

fn scenario(
    name: &'static str,
    cpus: usize,
    queues: Vec<QueueSpec>,
    procs: Vec<ProcSpec>,
) -> Scenario {
    Scenario {
        name,
        cpus,
        queues,
        procs,
        irqs: Vec::new(),
        protocol: Protocol::New,
        block: BlockShape::CommitInPass,
        window: WindowShape::PreemptOff,
        park: ParkShape::ReleaseLend,
        share: ShareShape::PerProcess,
        charge: ChargeShape::Honest,
        pass_cost_ns: 0,
        max_steps: 20_000,
        max_tasks: 32,
    }
}

fn process(name: &'static str, initial: Vec<usize>, templates: Vec<Script>) -> ProcSpec {
    ProcSpec {
        name,
        initial,
        templates,
        rt: false,
    }
}

/// The crash.md shape: a burst wake piles every worker onto the waker's CPU,
/// leaving a sibling idle and hungry, and the process tears down while that
/// sibling is reaching for one of them.
///
/// Under [`Protocol::New`] the reach is a message and the teardown is a
/// message, so the task is inside a container or inside a message at every
/// instant. Under [`Protocol::OldSteal`] it is on a stack, invisible to the
/// scan that concludes the process has no threads left.
pub fn crash_md_exit_race() -> Scenario {
    scenario(
        "crash_md_exit_race",
        2,
        vec![queue(WaitClass::Ipc)],
        vec![process(
            "app",
            vec![0, 1, 1, 1],
            vec![
                // main: signal the workers twice, so that at teardown time
                // they are spread across every state a task can be in —
                // running, queued, parked, and in transit between CPUs.
                Script::new(vec![
                    Op::Run(MS),
                    Op::Wake {
                        queue: 0,
                        all: true,
                        boost: None,
                    },
                    // Yield rather than hold the CPU for a full quantum: the
                    // workers have to actually run for the teardown to catch
                    // them spread across every state.
                    Op::Yield,
                    Op::Run(2 * MS),
                    Op::Yield,
                    Op::Wake {
                        queue: 0,
                        all: true,
                        boost: None,
                    },
                    Op::Run(MS),
                    Op::Yield,
                    Op::Run(MS),
                    Op::Teardown,
                ]),
                // worker
                Script::new(vec![
                    Op::Block {
                        queue: 0,
                        deadline: None,
                    },
                    Op::Run(2 * MS),
                    Op::Yield,
                    Op::Run(2 * MS),
                    Op::Block {
                        queue: 0,
                        deadline: None,
                    },
                    Op::Run(2 * MS),
                    Op::Exit,
                ]),
            ],
        )],
    )
}

/// The same workload driven with the OLD steal-and-scan algorithm. This is the
/// harness's self-validation gate (spec §10.3): it **must fail**. A fuzzer
/// that has never rejected the bug class it was built for proves nothing, so a
/// green run of everything else is only meaningful while this stays red.
pub fn old_steal_port() -> Scenario {
    let mut scenario = crash_md_exit_race().with_protocol(Protocol::OldSteal);
    scenario.name = "old_steal_port";
    scenario
}

/// The second harness self-validation gate, and the reason the block is two
/// steps: a port of the kernel's pre-`8508b37` blocking shape, where phase 2 of
/// the §8.1 handshake ran at the *call site* and the pass came after it.
///
/// It **must fail**. A remote waker that claims a task whose word already
/// reads `Blocked` posts `Msg::Wake` to the task's home CPU — the very CPU
/// about to park it — and that pass's own drain consumes the message while the
/// task is not in `parked` yet. On real hardware that was a panic plus a hang
/// on `--smp 8`, roughly twice in five audio suite runs.
///
/// Uses the `lost_wake_pipe` workload rather than a bespoke one: this is a
/// property of the *blocking shape*, not of a particular scenario, so the gate
/// should be the ordinary wait/wake workload with one thing changed.
pub fn old_commit_before_pass() -> Scenario {
    let mut scenario = lost_wake_pipe().with_block(BlockShape::CommitAtCallSite);
    scenario.name = "old_commit_before_pass";
    scenario
}

/// The same shape with the two halves fused into a single VM step — which is
/// what this simulator did until the split. Nothing can interleave, so the
/// window is not in the step relation and the run comes back clean.
///
/// It is the control for [`old_commit_before_pass`]: without it, "the harness
/// could not see this" is an assertion about a simulator nobody can run any
/// more. `blind_spot_needed_the_step_split` runs both.
pub fn old_commit_fused() -> Scenario {
    let mut scenario = lost_wake_pipe().with_block(BlockShape::CommitAtCallSiteFused);
    scenario.name = "old_commit_fused";
    scenario
}

/// The third harness self-validation gate: the kernel's registration window
/// with preemption left *enabled*, which is what it was until the ticket grew
/// a guard.
///
/// It **must abort** — not merely fail. Every other gate here is a verdict the
/// invariant walk returns; this one is the core's own `check_cpu` assertion
/// firing from inside a pass, because a task whose word reads `Committing`
/// while its CPU tries to preempt it has no legal transition to take. That is
/// the right failure and the reason the window has to be closed rather than
/// tolerated: `RunningTask::preempt` could be taught to accept `Committing`,
/// but the `Ready` word it would publish makes every waker that pops the
/// registration report `Claim::Lost` and move on — a lost wake, silently,
/// instead of a panic.
///
/// Run it with [`crate::explore::run_catching`]; `run` would take the abort
/// down with it.
///
/// The base workload is `crash_md_exit_race` rather than `lost_wake_pipe`,
/// and the reason is worth stating: reaching the window needs an interrupt
/// *delivered* into it, and the only messages that carry `Urgency::Preempt` —
/// the only ones that kick unconditionally — are the retire and an RT wake.
/// A plain pipe wake finds the waiter `Committing`, takes it with
/// `Claim::PrePark` and posts nothing, so the only way into the window there
/// is a quantum expiring, which needs ten foreign run chunks to elapse while
/// the blocked CPU declines its own pass. That is reachable in principle and
/// was not reached in 500 schedules.
pub fn old_preemptible_window() -> Scenario {
    let mut scenario = crash_md_exit_race().with_window(WindowShape::Preemptible);
    scenario.name = "old_preemptible_window";
    scenario
}

/// The five lost-wake windows (B3), one per source. Identical protocol,
/// different `WaitClass` and blocking shape — which is the point: the sources
/// stopped being different in the way that mattered.
fn lost_wake(name: &'static str, class: WaitClass, deadline: Option<u64>, all: bool) -> Scenario {
    const CONSUMERS: usize = 2;
    const ROUNDS: usize = 3;
    // At least one token per block, so the workload is satisfiable and a task
    // left parked at the end is a *lost wake* rather than an arithmetic
    // shortfall in the scenario.
    let wakes = CONSUMERS * ROUNDS;
    scenario(
        name,
        2,
        vec![queue(class)],
        vec![
            process(
                "producer",
                vec![0],
                vec![Script::looping(
                    vec![
                        Op::Run(MS),
                        Op::Wake {
                            queue: 0,
                            all,
                            boost: None,
                        },
                        // Without the yield the producer runs its whole
                        // script inside one quantum and the consumers never
                        // see an empty queue — a scenario that exercises
                        // nothing.
                        Op::Yield,
                    ],
                    wakes,
                )],
            ),
            process(
                "consumer",
                vec![0, 0],
                vec![Script::looping(
                    vec![Op::Block { queue: 0, deadline }, Op::Run(MS)],
                    ROUNDS,
                )],
            ),
        ],
    )
}

pub fn lost_wake_pipe() -> Scenario {
    lost_wake("lost_wake_pipe", WaitClass::Pipe, None, false)
}

/// With a deadline, so the wake and the local timeout arbitrate over the same
/// claim CAS — the arm that used to strand the second waiter (spec §8.2).
pub fn lost_wake_futex() -> Scenario {
    lost_wake("lost_wake_futex", WaitClass::Futex, Some(4 * MS), false)
}

pub fn lost_wake_iouring() -> Scenario {
    lost_wake("lost_wake_iouring", WaitClass::Io, Some(6 * MS), false)
}

pub fn lost_wake_listener() -> Scenario {
    lost_wake("lost_wake_listener", WaitClass::Ipc, None, true)
}

/// The audio shape: a device interrupt, not a thread, is the waker, and it
/// lends the woken client an RT window.
pub fn lost_wake_audio() -> Scenario {
    let mut scenario = scenario(
        "lost_wake_audio",
        2,
        vec![queue(WaitClass::Io)],
        vec![process(
            "client",
            vec![0, 0],
            vec![Script::looping(
                vec![
                    Op::Block {
                        queue: 0,
                        deadline: Some(10 * MS),
                    },
                    Op::Run(MS),
                ],
                3,
            )],
        )],
    );
    scenario.irqs.push(IrqSpec {
        period_ns: 3 * MS,
        queue: 0,
        boost_ns: Some(3 * MS),
    });
    scenario
}

/// B4: a task woken while its home CPU is on its way into `hlt`. The sleep
/// handshake is what keeps it from being slept through; if it were not, the
/// consumer would never be finalized and the run would quiesce with work
/// outstanding.
pub fn idle_hlt_race() -> Scenario {
    scenario(
        "idle_hlt_race",
        2,
        vec![queue(WaitClass::Pipe)],
        vec![
            process(
                "sleeper",
                vec![0],
                vec![Script::looping(
                    vec![
                        Op::Block {
                            queue: 0,
                            deadline: None,
                        },
                        Op::Run(MS / 4),
                    ],
                    4,
                )],
            ),
            process(
                "waker",
                vec![0],
                vec![Script::looping(
                    vec![
                        Op::Run(2 * MS),
                        Op::Wake {
                            queue: 0,
                            all: false,
                            boost: None,
                        },
                    ],
                    4,
                )],
            ),
        ],
    )
}

/// B7: an RT daemon woken by its device while a CPU hog holds the CPU, with a
/// preempt-off section in the hog to make the bound's `KernelSection` term
/// real rather than theoretical.
pub fn rt_wake_latency() -> Scenario {
    let mut scenario = scenario(
        "rt_wake_latency",
        2,
        vec![queue(WaitClass::Io)],
        vec![
            ProcSpec {
                name: "soundd",
                initial: vec![0],
                templates: vec![Script::looping(
                    vec![
                        Op::Block {
                            queue: 0,
                            deadline: Some(10 * MS),
                        },
                        Op::Run(MS / 2),
                    ],
                    4,
                )],
                rt: true,
            },
            process(
                "hog",
                vec![0, 0],
                vec![Script::looping(
                    vec![Op::Run(5 * MS), Op::KernelSection(MS / 2)],
                    4,
                )],
            ),
        ],
    );
    scenario.irqs.push(IrqSpec {
        period_ns: 3 * MS,
        queue: 0,
        boost_ns: None,
    });
    scenario
}

/// The whole audio path at once: an RT daemon driven by the device, two
/// clients it signals with a bounded priority boost, and a CPU hog trying to
/// eat the machine. `cpus = 1` is first-class here — it is the configuration
/// Doom actually runs in, and the one where every scheduling mistake is
/// audible.
pub fn audio_pipeline(cpus: usize) -> Scenario {
    let mut scenario = scenario(
        if cpus == 1 {
            "audio_pipeline"
        } else {
            "audio_pipeline_smp"
        },
        cpus,
        vec![queue(WaitClass::Io), queue(WaitClass::Pipe)],
        vec![
            ProcSpec {
                name: "soundd",
                initial: vec![0],
                templates: vec![Script::looping(
                    vec![
                        Op::Block {
                            queue: 0,
                            deadline: Some(6 * MS),
                        },
                        Op::Run(MS / 2),
                        // Signal the clients and lend them RT for one period.
                        Op::Wake {
                            queue: 1,
                            all: true,
                            boost: Some(3 * MS),
                        },
                    ],
                    4,
                )],
                rt: true,
            },
            process(
                "client",
                vec![0, 0],
                vec![Script::looping(
                    vec![
                        Op::Block {
                            queue: 1,
                            deadline: Some(12 * MS),
                        },
                        Op::Run(MS),
                    ],
                    4,
                )],
            ),
            process(
                "hog",
                vec![0],
                vec![Script::looping(vec![Op::Run(8 * MS), Op::Yield], 4)],
            ),
        ],
    );
    scenario.irqs.push(IrqSpec {
        period_ns: 3 * MS,
        queue: 0,
        boost_ns: None,
    });
    scenario
}

/// Many waiters, few tokens, every wait on a deadline: the shape where a
/// `wake_one` that lets a corpse consume it strands somebody forever.
pub fn futex_storm() -> Scenario {
    scenario(
        "futex_storm",
        2,
        vec![queue(WaitClass::Futex), queue(WaitClass::Futex)],
        vec![
            process(
                "waiters",
                vec![0, 0, 0, 1],
                vec![
                    Script::looping(
                        vec![
                            Op::Block {
                                queue: 0,
                                deadline: Some(2 * MS),
                            },
                            Op::Run(MS / 2),
                        ],
                        3,
                    ),
                    Script::looping(
                        vec![
                            Op::Block {
                                queue: 1,
                                deadline: Some(3 * MS),
                            },
                            Op::Run(MS / 2),
                        ],
                        3,
                    ),
                ],
            ),
            process(
                "wakers",
                vec![0],
                vec![Script::looping(
                    vec![
                        Op::Run(MS),
                        Op::Wake {
                            queue: 0,
                            all: false,
                            boost: None,
                        },
                        Op::Wake {
                            queue: 1,
                            all: false,
                            boost: None,
                        },
                    ],
                    4,
                )],
            ),
        ],
    )
}

/// Spawn placement and exit churn, which is where the ownership transfers
/// happen: every child is an `Adopt` that carries a task value between CPUs.
pub fn fork_storm() -> Scenario {
    scenario(
        "fork_storm",
        3,
        vec![queue(WaitClass::Other)],
        vec![process(
            "forker",
            vec![0],
            vec![
                Script::looping(
                    vec![
                        Op::Spawn { template: 1 },
                        Op::Spawn { template: 1 },
                        Op::Run(MS),
                    ],
                    4,
                ),
                Script::new(vec![Op::Run(2 * MS), Op::Yield, Op::Run(MS), Op::Exit]),
            ],
        )],
    )
}

/// Invariant I5's workload, and spec §11 Stage 9's gate: two processes of equal
/// entitlement and unequal thread count, both pure CPU, neither ever blocking.
///
/// Shape, and why each part of it:
///
/// * **Nothing blocks and nothing yields.** Fairness owes nothing across a
///   block, so I5 measures over contention windows; a workload with no blocks
///   is one window from the first dispatch to the first exit. Every other
///   scenario gives I5 windows a few milliseconds long, which is to say gives
///   it nothing to measure.
/// * **`solo` has one thread per CPU, `trio` three.** A fair share is per
///   *process* (spec §9.1), so they are owed the same CPU. Under any per-thread
///   policy `trio` takes three quarters instead of half — which is the whole
///   distinction, and is `fair_share_per_thread`.
/// * **Thread counts are multiples of `cpus`.** Spawn placement is
///   least-loaded-with-rotation, so each CPU ends up with the identical mix and
///   the run queues are balanced by construction. Balance-by-`StealRequest`
///   only answers a probe from a CPU whose victim has *two* ready tasks (spec
///   §7.7), so an odd thread count would leave a standing imbalance and this
///   would be measuring placement rather than fairness.
/// * **Each `solo` thread carries three times a `trio` thread's work**, so the
///   two processes have the *same total* work and, under an even split, finish
///   together. The window I5 measures over closes when the first process stops
///   being runnable, so this is what makes it the whole run rather than the
///   first third of it — and a bound that carries a quantum per thread needs a
///   window many quanta wide before a broken split can clear it.
pub fn fairness_storm(cpus: usize) -> Scenario {
    /// One `trio` thread's work: six quanta. `solo`'s threads run three times
    /// this, which is what equalizes the two processes' totals.
    const WORK: u64 = 60 * MS;
    let hog = |ns| Script::new(vec![Op::Run(ns)]);
    let mut scenario = scenario(
        if cpus == 1 {
            "fairness_storm"
        } else {
            "fairness_storm_smp"
        },
        cpus,
        // No wait queues: a queue nobody blocks on would only be scaffolding.
        Vec::new(),
        vec![
            process("solo", vec![0; cpus], vec![hog(3 * WORK)]),
            process("trio", vec![0; 3 * cpus], vec![hog(WORK)]),
        ],
    );
    scenario.max_tasks = 4 * cpus;
    scenario.max_steps = 4_000 + 4_000 * cpus;
    scenario
}

/// Negative gate for invariant I5, first of two: spec §13.9's rejected policy,
/// one fair share per *thread* instead of one per process.
///
/// It **must fail**. `trio` has three times `solo`'s threads and exactly the
/// same entitlement; under per-thread shares it takes three quarters of the
/// machine, and a fairness check that cannot see a 3:1 split of a two-way share
/// is not measuring fairness. The control is `fairness_storm` itself, which is
/// the identical workload under the shipped policy.
pub fn fair_share_per_thread() -> Scenario {
    let mut scenario = fairness_storm(1).with_share(ShareShape::PerThread);
    scenario.name = "fair_share_per_thread";
    scenario
}

/// Negative gate for invariant I5, second of two: `trio`'s share is charged
/// twice for every nanosecond it runs.
///
/// It **must fail**, and it fails in the *opposite* direction to
/// `fair_share_per_thread` — a share whose vruntime outruns its service is
/// throttled for work it never did, so `trio` ends up with a third of the
/// machine instead of half. The two gates together are what say I5 measures
/// service against entitlement rather than one side of it: the ordering could
/// be perfect and the charge wrong, or the charge perfect and the shares
/// mis-attributed, and both are unfair.
pub fn fair_double_charge() -> Scenario {
    let mut scenario = fairness_storm(1).with_charge(ChargeShape::Double { process: "trio" });
    scenario.name = "fair_double_charge";
    scenario
}

/// Negative gate for the core's `feature = "check"` pass-duration assert
/// (`cpu::MAX_PASS_NS`), which is the on-target counterpart to the simulator's
/// invariants (spec §10.2).
///
/// It **must abort**, like `old_preemptible_window`: the assert is the core's
/// own, so it unwinds rather than being recorded.
///
/// This one is not a port of a shape the kernel had — it cannot be, because the
/// thing being asserted is a *cost* and the simulator's clock does not advance
/// inside a step. It is calibration: `SimHw` charges every pass five times the
/// budget, and if the assert stays quiet then it is not compiled in, or it is
/// reading a clock that never moves, and every check build that ever came back
/// green certified nothing about how long a pass takes.
pub fn overlong_pass() -> Scenario {
    let mut scenario = lost_wake_pipe().with_pass_cost(5 * toyos_sched::cpu::MAX_PASS_NS);
    scenario.name = "overlong_pass";
    scenario
}

/// Every scenario the exit criterion covers, in the order the spec lists them.
/// `old_steal_port` and `old_commit_before_pass` are deliberately absent: they
/// are the negative gates, and a sweep that treated them as scenarios to pass
/// would be asserting the opposite of what they are for. `old_commit_fused` is
/// absent for the mirror-image reason — it passes, but only because the
/// harness cannot see the bug it contains.
pub fn all() -> Vec<Scenario> {
    vec![
        crash_md_exit_race(),
        lost_wake_pipe(),
        lost_wake_futex(),
        lost_wake_iouring(),
        lost_wake_listener(),
        lost_wake_audio(),
        idle_hlt_race(),
        rt_wake_latency(),
        audio_pipeline(1),
        audio_pipeline(2),
        futex_storm(),
        fork_storm(),
        fairness_storm(1),
        fairness_storm(2),
        old_park_kept_the_lend(),
    ]
}

/// Look a scenario up by name, for the CLI and the corpus replays.
pub fn by_name(name: &str) -> Option<Scenario> {
    // `fairness_storm:<cpus>` for any width, which is what spec §11 Stage 9
    // gates on ("1–128 vcpus"). `all()` carries only the two cheap widths.
    if let Some(cpus) = name.strip_prefix("fairness_storm:") {
        return cpus.parse().ok().filter(|&n| n >= 1).map(fairness_storm);
    }
    match name {
        "old_steal_port" => Some(old_steal_port()),
        "fair_share_per_thread" => Some(fair_share_per_thread()),
        "fair_double_charge" => Some(fair_double_charge()),
        "overlong_pass" => Some(overlong_pass()),
        "old_park_kept_the_lend" => Some(old_park_kept_the_lend()),
        "old_commit_before_pass" => Some(old_commit_before_pass()),
        "old_commit_fused" => Some(old_commit_fused()),
        "old_preemptible_window" => Some(old_preemptible_window()),
        _ => all().into_iter().find(|s| s.name == name),
    }
}

/// Negative gate for invariant I9: one lend, then a task that always blocks
/// before its quantum ends.
///
/// The victim is woken over and over by a waker that lends **nothing** — only
/// the very first wake carries a window. Under [`ParkShape::ReleaseLend`] that
/// window dies at the victim's first park and the victim spends the rest of the
/// run as a normal task. Under [`ParkShape::KeepLapsedLend`] — commit
/// `9c2fc4d`'s park — it survives every block, `RtState::arm` re-arms it at
/// every dispatch, and the victim runs at the borrowed priority forever off a
/// lend nobody renewed.
///
/// The victim's `Run(MS)` is deliberately far below the 10 ms quantum: that is
/// the whole point, since a task that ran a quantum would have its window
/// cleared at the preempt and the hole needs the *park*. Twenty iterations put
/// ~20 ms of boosted running time on one lend, comfortably past I9's bound, so
/// the gate fires early rather than on the last step.
pub fn old_park_kept_the_lend() -> Scenario {
    scenario(
        "old_park_kept_the_lend",
        1,
        vec![queue(WaitClass::Pipe)],
        vec![
            process(
                "victim",
                vec![0],
                vec![Script::looping(
                    vec![
                        Op::Block {
                            queue: 0,
                            deadline: Some(20 * MS),
                        },
                        Op::Run(MS),
                    ],
                    20,
                )],
            ),
            process(
                "waker",
                vec![0],
                vec![Script::new(vec![
                    // The one and only lend in the whole scenario.
                    Op::Wake {
                        queue: 0,
                        all: false,
                        boost: Some(3 * MS),
                    },
                ])],
            ),
            process(
                "renewer",
                vec![0],
                vec![Script::looping(
                    vec![
                        Op::Run(MS),
                        Op::Wake {
                            queue: 0,
                            all: false,
                            boost: None,
                        },
                    ],
                    20,
                )],
            ),
        ],
    )
}
