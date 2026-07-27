//! The scenario library — spec §11's Stage 4 row.
//!
//! Each scenario is a shape the kernel actually has, written as data. They are
//! deliberately small: the search space is the *interleaving*, not the
//! workload, and a scenario that takes ten thousand steps to quiesce buys one
//! schedule per second instead of a thousand.

use toyos_sched::task::WaitClass;

use crate::workload::{IrqSpec, Op, ProcSpec, Protocol, QueueSpec, Scenario, Script};

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

/// Every scenario the exit criterion covers, in the order the spec lists them.
/// `old_steal_port` is deliberately absent: it is the negative gate, and a
/// sweep that treated it as a scenario to pass would be asserting the
/// opposite of what it is for.
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
    ]
}

/// Look a scenario up by name, for the CLI and the corpus replays.
pub fn by_name(name: &str) -> Option<Scenario> {
    if name == "old_steal_port" {
        return Some(old_steal_port());
    }
    all().into_iter().find(|s| s.name == name)
}
