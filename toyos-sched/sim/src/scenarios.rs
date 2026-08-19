//! The scenario library — spec §11's Stage 4 row.
//!
//! Each scenario is a shape the kernel actually has, written as data. They are
//! deliberately small: the search space is the *interleaving*, not the
//! workload, and a scenario that takes ten thousand steps to quiesce buys one
//! schedule per second instead of a thousand.

use toyos_sched::queue::FairOrder;
use toyos_sched::task::WaitClass;

use crate::workload::{
    AgeShape, BlockShape, ChargeShape, IrqSpec, MigrateShape, Op, ParkShape, ProcSpec, Protocol,
    QueueSpec, Scenario, Script, ShareShape, WindowShape,
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
        migrate: MigrateShape::ReapTheCorpse,
        age: AgeShape::BoundedDeferral,
        share: ShareShape::PerProcess,
        charge: ChargeShape::Honest,
        order: FairOrder::InsertSequence,
        pass_cost_ns: 0,
        fair_allowance_ns: 0,
        thread_allowance_ns: 0,
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

/// A teardown that races the *balance* path rather than the wake path — the
/// shape the owner's T14 died in, at 949 s of uptime with doom exiting:
/// `retire_task: task not released after 1s: InTransit(CpuId(1))`.
///
/// Wide enough that CPUs go idle and probe while the teardown runs, and enough
/// workers that a victim can be surplus (`fair_len() > 1`) on the CPU the probe
/// reaches. That is the whole recipe: the thief asks, the victim CPU answers
/// from the *back* of its fair band — the end `pick` will not look at for a
/// long time — and the task it hands over is the one process teardown has just
/// killed.
///
/// Invariant I14 is what this exists to feed, and
/// [`old_migrate_kept_the_corpse`] is the gate that proves I14 has teeth.
pub fn retire_under_balance() -> Scenario {
    let mut scenario = scenario(
        "retire_under_balance",
        3,
        vec![queue(WaitClass::Io), queue(WaitClass::Pipe)],
        vec![
            // The audio daemon, verbatim from `audio_pipeline`: RT, driven by
            // the device, and it lends its clients a window when it signals
            // them. The lend is what reaches `CpuSched::place`'s forwarding
            // arm, which is the second caller of `hand_off` and the one that
            // needs no surplus to fire.
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
                        Op::Wake {
                            queue: 1,
                            all: true,
                            boost: Some(3 * MS),
                        },
                    ],
                    6,
                )],
                rt: true,
            },
            // The client, and the process that dies: a main thread that tears
            // down while its workers are parked on the queue soundd boosts.
            process(
                "client",
                vec![0, 1, 1, 1, 1],
                vec![
                    Script::new(vec![
                        Op::Run(MS),
                        Op::Yield,
                        Op::Run(4 * MS),
                        Op::Yield,
                        Op::Run(4 * MS),
                        Op::Teardown,
                    ]),
                    Script::looping(
                        vec![
                            Op::Block {
                                queue: 1,
                                deadline: Some(12 * MS),
                            },
                            Op::Run(MS),
                        ],
                        6,
                    ),
                ],
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

/// The fourth harness self-validation gate, and the one this file was extended
/// for: [`retire_under_balance`] with the balance path allowed to hand on a
/// task whose kill bit is already set.
///
/// It **must fail** invariant I14. `answer_steal_requests` pops from the back
/// of the fair band and migrates without reading the kill bit, so a task the
/// victim CPU would have dispatched at its own next `pick` becomes an
/// `Urgency::Normal` adopt aimed at a CPU that owes it nothing until its next
/// voluntary pass. Every other state a killed task can be in has an interrupt
/// behind the pass that handles it; `InTransit` has none, and this is the code
/// that put tasks there on purpose.
pub fn old_migrate_kept_the_corpse() -> Scenario {
    let mut scenario = retire_under_balance().with_migrate(MigrateShape::KeepTheCorpse);
    scenario.name = "old_migrate_kept_the_corpse";
    scenario
}

/// **One CPU, one permanently-RT thread that never parks, and a process that
/// dies underneath it** — the shape that turns `scheduler-core-spec.md` §3 into
/// a kernel panic if the real-time band's precedence over the dying list is
/// unqualified.
///
/// It is not a hypothetical workload. `Rights::RT` is capability-gated, but
/// `soundd` holds it in the shipped `system.toml` and `SYS_RT_ENTER` has no
/// revocation call anywhere in the tree, so an RT process that stops blocking is
/// one bug away — and every thread killed on its CPU then waits behind it. One
/// CPU is deliberate: `hand_off` refuses to migrate a killed task and
/// `pop_surplus` reads the fair band only, so a sibling CPU is no rescue and
/// pretending otherwise would only make the scenario slower to reach the point.
///
/// What invariant I14 reads here is the wall clock, which is the clock the
/// kernel's own tripwire reads — see [`crate::vm::Killed`].
pub fn rt_saturated_retire() -> Scenario {
    scenario(
        "rt_saturated_retire",
        1,
        vec![queue(WaitClass::Io), queue(WaitClass::Pipe)],
        vec![
            // The RT thread that stopped blocking. One block to let the
            // teardown happen underneath it, then a run long enough to outlast
            // invariant I14's whole bound — which is the point: if the band's
            // precedence over the dying list were unqualified, the corpse would
            // still be queued when this run ended.
            ProcSpec {
                name: "soundd",
                initial: vec![0],
                templates: vec![Script::new(vec![
                    Op::Block {
                        queue: 0,
                        deadline: Some(2 * MS),
                    },
                    Op::Run(300 * MS),
                ])],
                rt: true,
            },
            // The process that dies under it: a main thread that tears down
            // early, and a worker parked on a queue nothing ever signals, so
            // the retire is what makes it runnable and the whole of its unwind
            // is still owed when the RT band takes the CPU.
            process(
                "client",
                vec![0, 1],
                vec![
                    Script::new(vec![Op::Run(MS), Op::Teardown]),
                    Script::new(vec![Op::Block {
                        queue: 1,
                        deadline: Some(500 * MS),
                    }]),
                ],
            ),
        ],
    )
}

/// The tenth negative gate, and the second direction of the seventh: the same
/// workload with `pick` asking only `rq.has_rt()`, which is the shape this
/// branch shipped between the two fixes. The corpse never runs, `Hw::release`
/// is never called, and invariant I14 must say so.
///
/// `old_migrate_kept_the_corpse` is the other direction — a corpse handed away
/// and left waiting on a voluntary pass. Both are I14's, and a fix for either
/// that broke the other is exactly what this pair exists to stop.
pub fn old_rt_starved_the_corpse() -> Scenario {
    let mut scenario = rt_saturated_retire().with_age(AgeShape::RtOutranksEveryCorpse);
    scenario.name = "old_rt_starved_the_corpse";
    scenario
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

/// **The recorded fairness sample.** What the shipped scheduler's worst
/// invariant-I5 service spread over one contention window actually *is*, per
/// machine width — as opposed to what the derived bound says it should be.
///
/// This is the same two-tier shape gate A uses and for the same reason. The
/// bound in `invariants::check_fairness` is derived from the policy's own
/// granularity and is **not** moved when the shipped code misses it; that would
/// fit the gate to the implementation, and a gate fitted to what the code
/// already does cannot detect the code getting worse. So the derived bound stays
/// the standard, this table records where we are, and the two are compared on
/// every run: `Outcome::fair_over_bound` reports any crossing of the derived
/// bound whatever this table allows, so the allowance can hide a red suite but
/// never the gap.
///
/// Provenance — every number below came from this command at `be28bbd`, on one
/// host, with no other measurement running:
///
/// ```text
/// cargo run --release -p toyos-sched-sim -- measure fairness_storm:<cpus> 500
/// ```
///
/// | cpus | worst spread | derived bound | verdict |
/// |---|---|---|---|
/// | 1  |   30 ms |   60 ms | meets it, 2.0x |
/// | 2  |   84 ms |  108 ms | meets it, 1.3x |
/// | 3  |  125 ms |  156 ms | meets it, 1.2x |
/// | 4  |  198 ms |  204 ms | **crossed**, by 116 ms in some window |
/// | 6  |  324 ms |  300 ms | **crossed**, by 324 ms |
/// | 8  |  418 ms |  396 ms | **crossed**, by 418 ms |
/// | 12 |  634 ms |  588 ms | **crossed**, by 634 ms |
/// | 16 |  720 ms |  780 ms | meets it, 1.1x |
/// | 24 | 1056 ms | 1164 ms | meets it, 1.1x |
/// | 32 | 1386 ms | 1548 ms | meets it, 1.1x |
///
/// Widths 1 and 2 were additionally run at 10 000 seeds — the count `gate`
/// uses — giving 30 ms and 102 ms. One CPU is stable at 30 ms across 400, 500
/// and 10 000 seeds; two CPUs grows with the sample, which is what a
/// worst-of-N statistic does and why the allowance below carries a margin.
///
/// Two things this table says out loud. The fair split degrades as the machine
/// widens — 30 ms of spread at one CPU against 1386 ms at 32, which is not
/// noise and is filed as a known issue. And the shipped scheduler sits *on* its
/// own granularity bound at every width, crossing it at four of the ten
/// measured: the mechanism is saturating the limit its insertion-time keys
/// impose, rather than staying comfortably inside it.
///
/// **It is a bounded offset, not an accumulating drift**, which is the
/// difference between a granularity and a persistent unfair split. Measured by
/// scaling `WORK` and holding the seed count at 200: at one CPU the worst spread
/// is 30 ms at every window length; at eight it is 362 ms, 602 ms and 548 ms as
/// the window doubles and doubles again, so it saturates rather than growing
/// with time.
///
/// **And it is the policy's, not the model's.** Everything that decides who runs
/// next — `RunQueue`'s insertion-time keys, `FairShare`'s one vruntime pot per
/// process, `CpuSched::pick`, the surplus rule in `answer_steal_requests` — is
/// the shipped core; what the simulator mocks is time, timer, IPI, halt and
/// switch. The width scaling follows from §9.1 directly: every running thread of
/// a process charges the same pot, so the pot advances at the process's
/// aggregate rate while each queued thread's key stays frozen at its insert, and
/// one dispatch's worth of staleness therefore buys more wall-clock service the
/// more of that process is running at once. What the *model* contributes is the
/// search: these are worst-of-N figures over adversarially chosen interleavings
/// (seeded and PCT), not the split hardware would show on an average schedule.
const FAIRNESS_SAMPLE: &[(usize, u64)] = &[
    (1, 30 * MS),
    (2, 102 * MS),
    (3, 125 * MS),
    (4, 198 * MS),
    (6, 324 * MS),
    (8, 418 * MS),
    (12, 634 * MS),
    (16, 720 * MS),
    (24, 1056 * MS),
    (32, 1386 * MS),
];

/// The ceiling a `fairness_storm` run is *gated* on: the recorded sample plus a
/// quarter. That margin is what makes this a regression test rather than a
/// transcription of one afternoon's tail — the worst-of-N spread grew from 84 ms
/// to 102 ms between 500 and 10 000 seeds at two CPUs, and a ceiling with no
/// headroom would red on sample size alone.
///
/// What it can detect, stated the way gate A's fast tier states it: a fairness
/// regression that widens the worst spread by more than 25%. Not a subtle one.
/// A width with no recorded sample gets **zero**, so the derived bound governs
/// it — an allowance is a claim that a measurement was taken, and nobody has
/// taken one there.
fn fair_allowance(cpus: usize) -> u64 {
    FAIRNESS_SAMPLE
        .iter()
        .find(|(width, _)| *width == cpus)
        .map_or(0, |(_, worst)| worst + worst / 4)
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
    let mut scenario = fair_workload(
        if cpus == 1 {
            "fairness_storm"
        } else {
            "fairness_storm_smp"
        },
        cpus,
        WORK,
    );
    scenario.fair_allowance_ns = fair_allowance(cpus);
    scenario
}

/// One `trio` thread's work in [`fairness_storm`]: six quanta. `solo`'s threads
/// run three times this, which is what equalizes the two processes' totals.
const WORK: u64 = 60 * MS;

/// The shape both fairness workloads share, with the per-thread work as the one
/// parameter — see [`fairness_storm`] for why every other part of it is what it
/// is, and [`sibling_storm`] for why one of the two needs longer threads.
///
/// **The recorded per-thread sample.** What the shipped scheduler's worst
/// invariant-I13 service spread over one contention window actually *is*, per
/// machine width, against the 60 ms the bound derives from the fair band's own
/// granularity (five dispatches of one run queue: four rivals plus the leader,
/// at `QUANTUM + 2 × RUN_CHUNK` each). Every number came from
///
/// ```text
/// cargo run --release -p toyos-sched-sim -- measure fairness_storm:<cpus> <seeds>
/// ```
///
/// on `be4b34a` with this change applied, on one host, with no other
/// measurement running:
///
/// | cpus | seeds | worst I13 spread | worst I5 spread, same runs |
/// |---|---|---|---|
/// | 1  | 10 000 | 10 ms |   30 ms |
/// | 2  | 10 000 | 30 ms |  102 ms |
/// | 3  |    500 | 28 ms |  125 ms |
/// | 4  |    500 | 28 ms |  198 ms |
/// | 6  |    500 | 31 ms |  324 ms |
/// | 8  |    500 | 32 ms |  418 ms |
/// | 12 |    200 | 35 ms |  634 ms |
/// | 16 |    200 | 37 ms |  612 ms |
/// | 24 |    200 | 42 ms | 1046 ms |
/// | 32 |    200 | 50 ms | 1386 ms |
///
/// `sibling_storm` itself, at 300 seeds: 10 ms.
///
/// Two things the table says out loud. **No width crosses the derived bound**,
/// so no width has a `thread_allowance_ns` — an allowance is a licence to sit
/// above the standard and nothing here needs one. Handing out the sample plus a
/// quarter regardless, the way [`fair_allowance`] does, would put the 32-CPU
/// ceiling at 62 ms against a derived 60: the allowance quietly becoming the
/// standard, which is the exact failure the two-tier shape exists to prevent.
/// If a width ever does cross, its allowance goes on the scenario and
/// `Outcome::thread_over_bound` reports the gap on every run regardless.
///
/// And **the per-thread split does not degrade as the machine widens** — 10 ms
/// at one CPU against 50 ms at 32, against I5's 30 ms against 1386 ms over the
/// identical runs. The width degradation filed as a known issue is a
/// *per-process* phenomenon; inside a share the ordering holds flat, which is
/// what the insertion-time keys of one monotone pot are worth. The slow climb
/// is worth watching: at 32 CPUs the spread is 83% of the bound, and no width
/// above 32 has been measured.
fn fair_workload(name: &'static str, cpus: usize, work: u64) -> Scenario {
    let hog = |ns| Script::new(vec![Op::Run(ns)]);
    let mut scenario = scenario(
        name,
        cpus,
        // No wait queues: a queue nobody blocks on would only be scaffolding.
        Vec::new(),
        vec![
            process("solo", vec![0; cpus], vec![hog(3 * work)]),
            process("trio", vec![0; 3 * cpus], vec![hog(work)]),
        ],
    );
    scenario.max_tasks = 4 * cpus;
    // Quadratic in the machine, measured: 440 steps at one CPU, 980 at two,
    // 71k at 32 and 265k at 64. The *work* is linear in `cpus` (360 run chunks
    // each), so the rest is idle passes, steal probes and IPI deliveries, which
    // is a per-CPU cost paid against every other CPU. A safety net sized on the
    // linear term alone reports non-termination on a run that was progressing
    // fine, which is what the first version of this line did at 64 and 128.
    scenario.max_steps = 20_000 + (work / WORK) as usize * 20_000 + 100 * cpus * cpus;
    scenario
}

/// Invariant I13's workload: [`fairness_storm`] at one CPU with threads five
/// times as long.
///
/// The length is the whole difference, and it is derived rather than picked.
/// I5's bound grows with the machine's thread count, so `fairness_storm`'s
/// six-quantum threads are already many times it; I13's bound is a *per-CPU*
/// constant — 60 ms, five dispatches of one run queue's fair band — and six
/// quanta of work per thread means a completely starved sibling separates from
/// a completely served one by 60 ms and then exits. That is the bound exactly,
/// and a gate that can only reach its own threshold is not a gate. Five times
/// the work makes total starvation a 300 ms separation, so the two negative
/// gates below are caught on the way to the failure rather than at it.
///
/// One CPU, because the property is a property of one run queue: the fair band
/// orders the threads that are in *it*, and a second CPU adds placement, which
/// I13's window deliberately declines to measure across.
pub fn sibling_storm() -> Scenario {
    fair_workload("sibling_storm", 1, 5 * WORK)
}

/// Negative gate for invariant I13, first of two: the fair band's tie-break
/// switched from the monotonic insertion sequence to `TaskKey` — the identity
/// tie-break `queue.rs` warns against in the comment beside the field.
///
/// **It passes, and that is the finding.** The warning is written as though the
/// tie-break were what round-robins a share's threads; it is not. A share's pot
/// is charged for every nanosecond any of its threads runs, so a thread
/// re-inserted after a dispatch carries a key strictly above every sibling
/// queued before it and the ordering is already insertion order *without* the
/// tie-break doing anything. Exact ties survive only where no charge separates
/// two inserts — a `wake_all` of siblings, or the spawn burst — and one
/// dispatch dissolves them. Measured, not argued: 300 seeds of
/// [`sibling_storm`] under this ordering are clean, and the same 300 under
/// [`fair_identity_within_share`] fail on I13 every time.
///
/// It is kept for the reason `old_commit_fused` is kept: it is the control that
/// turns "the obvious break is invisible here" from a claim into a measurement,
/// and it is what says the second gate below had to be written the way it is.
pub fn fair_identity_tiebreak() -> Scenario {
    let mut scenario = sibling_storm().with_order(FairOrder::IdentityTiebreak);
    scenario.name = "fair_identity_tiebreak";
    scenario
}

/// Negative gate for invariant I13, second of two, and the one with teeth: the
/// `queue.rs` warning made total. Whichever share leads the fair band, its
/// *lowest-keyed* ready thread is dispatched — so the same thread wins every
/// time, not merely every tie.
///
/// It **must fail**, and it must fail on I13 *alone*. That second half is the
/// point of the whole check: the share's pot advances at exactly the rate it
/// did before, because it is charged for the time the process ran and not for
/// which of its threads ran it. `solo` and `trio` split the machine as evenly as
/// ever and invariant I5 — which measures service per *process* — sees a
/// perfectly fair scheduler while two of `trio`'s three threads never run at
/// all. This is the hole I13 was built for, and the redesign it has to guard
/// (an ordered map of shares, each holding a FIFO of its ready threads) puts
/// precisely this code path at the centre of the scheduler.
pub fn fair_identity_within_share() -> Scenario {
    let mut scenario = sibling_storm().with_order(FairOrder::IdentityWithinShare);
    scenario.name = "fair_identity_within_share";
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

/// Negative gate for the core's `feature = "check"` pass-cost recorder
/// (`cpu::PassCosts`, budget `cpu::MAX_PASS_NS`), which is the on-target
/// counterpart to the simulator's invariants (spec §10.2).
///
/// **It must be recorded, not aborted.** The recorder replaced an assert: a
/// pass's elapsed time is wall clock across the pass, and a guest's wall clock
/// advances while a hypervisor has its vCPU, so the quantity carries a term the
/// kernel neither observes nor controls and no panic may stand over it. What
/// stands over it is a measurement, judged where composed quantities are judged.
///
/// This one is not a port of a shape the kernel had — it cannot be, because the
/// thing being measured is a *cost* and the simulator's clock does not advance
/// inside a step. It is calibration: `SimHw` charges every pass five times the
/// budget, and if the recorder comes back empty or under budget then it is not
/// compiled in, or it is reading a clock that never moves, and every check build
/// that ever came back green certified nothing about how long a pass takes.
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
        retire_under_balance(),
        rt_saturated_retire(),
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
        sibling_storm(),
        lend_then_block(),
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
        "old_migrate_kept_the_corpse" => Some(old_migrate_kept_the_corpse()),
        "old_rt_starved_the_corpse" => Some(old_rt_starved_the_corpse()),
        "fair_share_per_thread" => Some(fair_share_per_thread()),
        "fair_double_charge" => Some(fair_double_charge()),
        "fair_identity_tiebreak" => Some(fair_identity_tiebreak()),
        "fair_identity_within_share" => Some(fair_identity_within_share()),
        "overlong_pass" => Some(overlong_pass()),
        "old_park_kept_the_lend" => Some(old_park_kept_the_lend()),
        "lend_then_block" => Some(lend_then_block()),
        "old_commit_before_pass" => Some(old_commit_before_pass()),
        "old_commit_fused" => Some(old_commit_fused()),
        "old_preemptible_window" => Some(old_preemptible_window()),
        _ => all().into_iter().find(|s| s.name == name),
    }
}

/// Invariant I9's workload, and the control half of its gate: one lend, then a
/// task that always blocks before its quantum ends.
///
/// The victim is woken over and over by a waker that lends **nothing** — only
/// the very first wake carries a window. Under [`ParkShape::ReleaseLend`], which
/// is what this carries and what the kernel ships, that window dies at the
/// victim's first park and the victim spends the rest of the run as a normal
/// task. [`old_park_kept_the_lend`] is the same workload under commit
/// `9c2fc4d`'s park and is the negative gate.
///
/// The victim's `Run(MS)` is deliberately far below the 10 ms quantum: that is
/// the whole point, since a task that ran a quantum would have its window
/// cleared at the preempt and the hole needs the *park*. Twenty iterations put
/// ~20 ms of boosted running time on one lend, comfortably past I9's bound, so
/// the gate fires early rather than on the last step.
pub fn lend_then_block() -> Scenario {
    scenario(
        "lend_then_block",
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

/// Negative gate for invariant I9: [`lend_then_block`] under commit `9c2fc4d`'s
/// park, which cleared the borrowed window only `if now >= until`.
///
/// It **must fail**. A lend blocked on before it ran out survives the block, and
/// with `RtState::arm` re-arming at every dispatch a task that obtains one lend
/// and thereafter runs less than a quantum before blocking holds inherited RT
/// forever — off a single pipe interaction, with nobody renewing anything.
///
/// The I9 that shipped alongside that park could not see it, and the giveaway
/// was that it needed no change: it compared a *running* task's `until` against
/// the clock, and a re-armed `until` is by construction fresh. A check that
/// passes because it stopped measuring is gate A's instrument-defect shape, so
/// I9 is the cumulative form now and this is what says so.
///
/// **A named constructor rather than a `with_park` at one call site**, which is
/// what it was until the CLI's `gate` was found to be running eight of the nine
/// negative gates: a gate reachable only through a modifier one test applies is
/// a gate the exit criterion cannot name.
pub fn old_park_kept_the_lend() -> Scenario {
    let mut scenario = lend_then_block().with_park(ParkShape::KeepLapsedLend);
    scenario.name = "old_park_kept_the_lend";
    scenario
}
