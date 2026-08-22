---
status: owner
kind: question
opened: 2026-08-22
---

# An idle CPU that slept before the surplus appeared is never probed again

The balance path is pull-only. `SchedPass::post_steal_probe`
(`toyos-sched/src/cpu.rs`) runs on the idle path, picks the CPU publishing the
most surplus, and returns without posting anything if that surplus is under two.
The probe node is one-shot, so there is at most one outstanding per CPU, and
nothing re-posts it: `driver::execute`'s `Action::Idle` arm halts the CPU, and a
CPU with no run queue, no deadline and no device work stays halted until an
interrupt arrives. There is no push half — a CPU that gains surplus tells
nobody.

So a CPU that reached its idle pass while every other CPU still published a
surplus of zero, and then halted, is not given work by the balance path however
much surplus appears afterwards. What recovers it is a kick from somewhere else,
and every path that produces one is a path that was already giving that CPU the
work: spawn placement kicks the CPU it placed on, a wake kicks the waiter's home
CPU.

Measured, `toyos-sched/sim/tests/policy.rs`'s
`the_balance_path_drains_the_cpu_an_adversary_loaded` over 20 seeds per width,
with every thread spawned onto cpu0 (`workload::PlacementShape::AllOn`):

| cpus | threads | seeds reaching every CPU | seeds reaching a second CPU |
|---|---|---|---|
| 2 |  8 | 9/20 |  9/20 |
| 4 | 16 | 2/20 | 13/20 |
| 4 | 64 | 2/20 | 13/20 |
| 8 | 64 | 0/20 | 15/20 |

Where a thief was awake the drain is complete — the best seed at every width
moves exactly `threads − 2` tasks, cpu0's whole surplus above the floor
`answer_steal_requests` stops at. The first column is the CPUs that lost the
race, and it falls with width because all of them have to win it.

**Reachable in the kernel, not only in the model.** A wake posts to the waiter's
*home* CPU whatever that CPU's load is, and a home is where spawn placement put
the thread when it was created. A process whose threads were all placed on one
CPU while the machine was otherwise busy, parked, and then woken together, is
exactly the state above: one CPU with N runnable threads and the sleepers with
no probe outstanding. Nothing has measured how often that composition occurs on
a real boot.

Not filed as a defect because no shipped workload has been shown to reach it,
and the fix is a design decision the balance path does not currently make: a
push on surplus, or a bounded re-arm of the probe, both of which cost an idle
CPU wakes it is currently right to be without
(`kernel/CLAUDE.md`: anything added to the idle loop is an audio change).

## Decision owed: which cure, if either, the balance path takes

Both cures are now implemented in `toyos-sched` behind `cpu::Balance`, which the
simulator selects and `kernel::sched::driver::env` does not: it selects
`Balance::Pull` and the kernel's behaviour is unchanged. The measurements are
`toyos-sched/sim/tests/policy.rs`'s three new cases, 20 seeds per point
(`cargo test -p toyos-sched-sim --test policy -- --nocapture`).

**`Balance::PullWithRearm { every_ns, times }`** — a CPU that halts with nothing
to run arms its one-shot timer `every_ns` ahead and probes again when it fires,
up to `times` times per idle period, the allowance refilling at every dispatch.
It observes nothing and needs nobody's cooperation.

**`Balance::PushOnSurplus { threshold }`** — a pass that publishes a surplus of
`threshold` or more rings the doorbell of one CPU that reads SLEEPING, walking
them in turn. No task moves on the ring; the woken CPU posts an ordinary probe.

Recovery on the lopsided machine, at `every_ns = QUANTUM_NS` and `threshold = 2`
— seeds reaching every CPU, and the worst time a halted CPU sat beside a
published surplus with no probe outstanding:

| cpus/threads | pull | re-arm ×1 | re-arm ×4 | push ≥2 |
|---|---|---|---|---|
| 2 /  8 | 9/20, 480 ms | 20/20, 10 ms | 20/20, 10 ms | 20/20,  1 ms |
| 4 / 16 | 2/20, 960 ms | 20/20, 10 ms | 20/20, 10 ms | 20/20, 22 ms |
| 4 / 64 | 2/20, 3.84 s | 20/20, 10 ms | 20/20, 10 ms | 20/20, 22 ms |
| 8 / 64 | 0/20, 3.84 s | 20/20, 10 ms | 20/20, 10 ms | 20/20, 66 ms |

Cost, in wakes of a halted CPU that found nothing to do — the whole sweep's
count, and the worst single run's rate against simulated time:

| workload | pull | re-arm ×1 | re-arm ×4 | push ≥2 |
|---|---|---|---|---|
| `interactive_mix(2,4)`  | 0, 0.00/s |  44,  31.58/s |  171,  88.00/s |   9,  11.76/s |
| `interactive_mix(2,16)` | 0, 0.00/s |  49,  11.94/s |  174,  30.14/s |   2,   3.08/s |
| `interactive_mix(4,16)` | 0, 0.00/s | 104,  20.90/s |  363,  63.01/s |  53,  24.62/s |
| `wakeup_storm(4,16)`    | 0, 0.00/s | 251,  92.13/s | 1004, 306.22/s |  80,  97.56/s |
| `wakeup_storm(8,64)`    | 0, 0.00/s | 521, 150.67/s | 2075, 534.78/s | 748, 380.95/s |
| `audio_pipeline(4)`     | 0, 0.00/s | 111, 153.85/s |  351, 243.90/s | **0, 0.00/s** |

The audio-relevant latency does not move: the watched thread's worst wake is the
same nanosecond under all four policies at every point but `wakeup_storm(8,64)`,
where the push's 50,750,000 ns beats the shipped path's 57,000,000 ns, and the
means move by under 2%. The model charges a pass zero nanoseconds, so it counts
an extra wake and cannot bill one — **what a wake costs the CPU it wakes is a
kernel measurement nobody has taken.** That is the one number this decision is
short of.

**The push costs an ordering the pull path does not have.** It is the
store-buffer litmus test: the pusher stores its surplus and loads the sleeper's
SLEEPING bit while the sleeper stores SLEEPING and loads the surplus, and without
a `SeqCst` fence between each side's store and its load both may miss — the same
defect in a window nanoseconds wide. `cpu::balance_fence` is that fence, an
`mfence` on the exit of every pass that has surplus, and
`toyos-sched/loom/tests/loom_push.rs` is the model;
`cargo test -p toyos-sched-loom --features push-fence-relaxed --test loom_push`
weakens it and the model reds. The re-arm needs no such edge at all: a timer
fires whether or not anybody published anything.

**Recommendation: `PushOnSurplus { threshold: 2 }`, or nothing.** It recovers the
machine at every width, it costs the audio pipeline exactly zero extra idle
wakes because a machine with no surplus never pushes, and it is self-limiting in
the direction that matters — the wake only ever happens where there is work to
come and get. The re-arm buys the same recovery with a periodic tick on every
idle CPU, including on a machine that has nothing at all to hand out; on the
audio workload that is 154 wakes per second bought for nothing. What the push
costs instead is an `mfence` on the pass exit of a CPU with surplus and a second
read of the surplus on the idle path — both off the audio pipeline's hot path,
because that workload never has surplus. Doing nothing also remains defensible:
no shipped workload has been shown to reach the state, and `kernel/CLAUDE.md`'s
rule is that the idle path stays empty until something forces it open.

Two things would change the answer, and neither is measured yet: what one wake
actually costs a woken CPU in the kernel (the model cannot say), and how often a
real boot produces the composition in "Reachable in the kernel" above.
