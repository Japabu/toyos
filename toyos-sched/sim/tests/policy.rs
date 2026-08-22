//! **The measured policy suite**: what the scheduler's policy actually delivers,
//! as numbers, against bounds derived from its own constants.
//!
//! `scenarios.rs` next door is the exit criterion — every negative gate fails,
//! every scenario passes. That is a suite of *verdicts*, and the external review
//! of 2026-08-20 named what it leaves out: the policy this scheduler states —
//! "threads execute, processes own fair share" — is encoded in implementation
//! intent and was validated nowhere empirically. A verdict cannot say how much
//! share a process actually got, how long an interactive wake actually waited,
//! or how long a runnable task can actually be passed over. Those are the
//! quantities a user feels, and each one below is measured and then asserted.
//!
//! # The one constant every bound here is made of
//!
//! Every case in this file lands on the same term, and it is not a coincidence:
//!
//! ```text
//! (runnable threads on one CPU + 1) × (QUANTUM_NS + max KernelSection + 2 × RUN_CHUNK_NS)
//! ```
//!
//! The fair band is keyed by the vruntime a thread held **when it was inserted**
//! (`queue.rs`, spec §9.2), and a share's pot advances only when one of its
//! threads runs. So a thread queued behind `R` others can be passed over by all
//! `R` of them on the strength of keys that were already stale when its own wait
//! began, and the leader spends one more quantum on top. It is invariant I5's
//! staleness term and invariant I13's whole bound, and this file is the finding
//! that it is *also* the interactive wake latency, *also* the starvation bound,
//! and *also* exactly what a thread-count attack is worth. The kernel-section
//! and chunk terms are I9's, for I9's reason: a preempt-off section overruns the
//! quantum it started in, and the model observes an expiry one chunk late.
//!
//! # What each case measures, and what it found
//!
//! | case | measured | derived bound | measured/bound |
//! |---|---|---|---|
//! | thread-count asymmetry | `solo` finishes `N+1` quanta late, at every `N` | `(N+2) × 12 ms` | 0.82 at N=64 |
//! | the deficit over time | 50 ms at N=4, 170 ms at N=16, unchanged as the window doubles twice | an offset, not a drift | — |
//! | interactive wake | 637 ms worst at 64 hogs; every wake but one at 0 ns | `(rivals+1) × 12 ms` = 792 ms | 0.80 |
//! | wakeup storm drain | 27.75 ms for 64 waiters on one CPU, 3.1× the 16-waiter figure | `(per queue+1) × 12 ms` = 780 ms | 0.04 |
//! | starvation | 70 ms at 5 runnable threads, 680 ms at 65 | `(threads+1) × 12 ms` | 0.97 / 0.86 |
//! | adversarial placement | a CPU an adversary loaded is drained to the surplus floor, and only the CPUs still awake are reached | `threads − 2` stealable | 1.00 |
//! | a bounded re-arm | every CPU is reached at every width; 10 ms of probe gap | `every_ns + 3 × RUN_CHUNK` = 13 ms | 0.77 |
//! | a push on surplus | every CPU is reached at every width; 66 ms of probe gap at eight | `(cpus−1) × 12 ms + IPI + 2 × RUN_CHUNK` | 0.77 |
//! | what the cures cost | 0 idle wakes/s shipped, 154/s for the cheapest re-arm and **0/s** for the push on `audio_pipeline` | wake latency unchanged, under its own bound | — |
//!
//! The adversarial-placement case is the only one whose bound is a *count*
//! rather than a duration, and that is the model's limit rather than a choice:
//! the clock advances on the executing CPU's step, so a run's makespan is what
//! one CPU would take at every width and no wall-clock statement about a wide
//! machine can be read off it. What survives is the protocol — how many tasks
//! the balance path moved, and which CPUs ever got one — and that is enough to
//! separate a machine that recovers from one that does not. The two cure cases
//! below it inherit the same limit and answer it the same way: recovery is
//! counted in CPUs, and timed in **probe gaps** — how long a halted CPU sat
//! beside a published surplus with no probe of its own outstanding, which is a
//! quantity the protocol owns end to end.
//!
//! Every number came from this file at 16–40 seeds per point:
//! `cargo test -p toyos-sched-sim --test policy -- --nocapture`. The tables in
//! each test carry the full sweeps.
//!
//! # What the numbers say, in one paragraph
//!
//! **A process cannot buy CPU by forking, and the qualifier is a window
//! length.** `swarm` with N threads delays a single-threaded rival by exactly
//! `N+1` quanta — once — and never again however long the run goes on. Over the
//! 120 ms `solo` would take against one rival that is a fall from a 500‰ share
//! to 77‰ at N=64; over a window four times as long the same deficit is 453‰ at
//! N=4. So the policy's claim holds asymptotically and is a granularity
//! statement, not a guarantee about any particular window — which is the honest
//! form of "processes own fair share", and it is measured here rather than
//! asserted anywhere.
//!
//! **The balance path is pull-only and one-shot, and both cures for that work.**
//! An idle CPU posts one probe on its way to `hlt` and nothing re-posts it, so a
//! CPU that halted before any neighbour published surplus is never probed again:
//! 0 of 20 seeds reach every CPU at eight. A bounded re-arm of the probe and a
//! push on surplus each take that to 20 of 20 at every width, and they cost
//! opposite things — the timer ticks whether or not there is anything to come
//! for, the push fires only where there is. On the audio pipeline that is 154
//! extra idle wakes per second against **zero**. Neither is shipped;
//! `issues/kernel/an-idle-cpu-that-slept-before-the-surplus-is-never-probed.md`
//! is where the decision sits, and these three cases are the numbers it is owed
//! against.
//!
//! # Determinism, seeds, and what varies
//!
//! The simulator is deterministic in its decision stream: a seed is a schedule.
//! Every case sweeps seeds 0..N alternating the uniform driver with PCT, which is
//! the idiom `scenarios.rs` uses and the only source of variation the tree has —
//! there is no wall clock and no host randomness anywhere in a run. Two of the
//! workloads sit at one CPU, where the enabled-step set is nearly a singleton and
//! every seed produces the *same* run: `share_gain`'s worst and best across 16
//! seeds are the identical nanosecond at every width, and that is reported rather
//! than hidden, because a worst-of-N over N identical runs is a worst-of-one and
//! a reader is owed that. The multi-CPU cases do vary, and there the sweep is
//! doing what a sweep is for.

use toyos_sched::cpu::Balance;
use toyos_sched::fair::QUANTUM_NS;
use toyos_sched_sim::choice::ChoiceStream;
use toyos_sched_sim::explore::{run, Outcome};
use toyos_sched_sim::latency::{Latency, ReadyCause};
use toyos_sched_sim::scenarios;
use toyos_sched_sim::vm::{IPI_LATENCY_NS, RUN_CHUNK_NS};
use toyos_sched_sim::workload::{Scenario, ShareShape};

const MS: u64 = 1_000_000;

/// One thread's work in the share-gain cases: six quanta, the same figure
/// `scenarios::WORK` gives a `fairness_storm` thread and for the same reason —
/// a window many quanta wide is what a granularity bound needs before it can
/// separate a fair split from a broken one.
const WORK: u64 = 60 * MS;

/// Seeds per point for the one-CPU cases. They are deterministic (see the module
/// header), so this buys reproducibility of the *reported* number rather than
/// search; the cases that actually explore say so where they set their own.
const SEEDS: u64 = 16;

/// The fair band's granularity: one dispatch of one run queue. Every bound in
/// this file is a multiple of it.
///
/// `max KernelSection` is zero in all three policy workloads — none of them runs
/// an `Op::KernelSection` — so the term is dropped here rather than carried as a
/// zero, and any workload that grows one has to add it back.
const DISPATCH_NS: u64 = QUANTUM_NS + 2 * RUN_CHUNK_NS;

fn stream(scenario: &Scenario, seed: u64) -> ChoiceStream {
    if seed.is_multiple_of(2) {
        ChoiceStream::from_seed(seed)
    } else {
        ChoiceStream::pct(seed, scenario.cpus, 3)
    }
}

/// Run one scenario over `seeds` schedules, asserting every invariant walk stays
/// clean, and hand each outcome to `fold`. Every case here is a measurement over
/// runs that also had to be *correct*, and a policy number taken off a run that
/// violated I1 would be a number about a broken machine.
fn sweep(scenario: &Scenario, seeds: u64, mut fold: impl FnMut(&Outcome)) {
    for seed in 0..seeds {
        let mut choices = stream(scenario, seed);
        let outcome = run(scenario.clone(), &mut choices);
        assert!(outcome.passed(), "{}", outcome.report());
        fold(&outcome);
    }
}

/// **The share-gain attack, measured**: what a process buys by having N runnable
/// threads instead of one.
///
/// `solo` has one thread and `swarm` has N, both pure CPU on one CPU, both
/// entitled to half of it. `solo`'s work is a fixed 60 ms, so the instant it
/// finishes *is* the share it was served at: 120 ms means half the CPU, 240 ms
/// means a quarter.
///
/// **What it found.** `solo` finishes exactly `N+1` quanta late at every width
/// above one, and the seeds do not disagree by a nanosecond
/// (`cargo test -p toyos-sched-sim --test policy -- --nocapture`):
///
/// | N | T_solo | deficit | in quanta | `solo`'s share over that window |
/// |---|---|---|---|---|
/// | 1  | 120 ms |   0 ms |  0 | 500‰ |
/// | 2  | 150 ms |  30 ms |  3 | 400‰ |
/// | 4  | 170 ms |  50 ms |  5 | 352‰ |
/// | 8  | 210 ms |  90 ms |  9 | 285‰ |
/// | 16 | 290 ms | 170 ms | 17 | 206‰ |
/// | 32 | 450 ms | 330 ms | 33 | 133‰ |
/// | 64 | 770 ms | 650 ms | 65 |  77‰ |
///
/// So the attack **works, once, and by exactly the fair band's staleness**: at
/// t=0 every thread is inserted at vruntime 0, `solo` is dispatched first and
/// re-inserted with its share's pot at 10 ms, and every one of `swarm`'s N
/// threads still holds the stale key it was queued with — so all N run a quantum
/// ahead of `solo`'s second turn. After that burst each swarm thread carries a
/// key its own dispatch set, and the two processes alternate one for one.
///
/// **The bound is derived and it is not moved.** `(N + 2)` dispatches of the one
/// run queue — the N+1 runnable threads and the leader's extra quantum, which is
/// invariant I5's `(ΣT + 1)` term exactly. The measured deficit fills 0.63 of it
/// at N=2 and climbs to 0.82 at N=64, so the scheduler is closing on its own
/// granularity as the swarm widens rather than living comfortably inside it —
/// which is the same thing `scenarios::FAIRNESS_SAMPLE` records about I5.
#[test]
fn a_process_cannot_buy_cpu_by_forking() {
    let mut table = Vec::new();
    for threads in [1usize, 2, 4, 8, 16, 32, 64] {
        let scenario = scenarios::share_gain(threads, WORK);
        let solo = scenario
            .process_index("solo")
            .expect("share_gain has a solo");
        let bound = 2 * WORK + (threads as u64 + 2) * DISPATCH_NS;
        let (mut worst, mut best) = (0, u64::MAX);
        sweep(&scenario, SEEDS, |outcome| {
            let finish = outcome.process_finish_ns[solo]
                .expect("solo's 60 ms of work must complete inside the run");
            worst = worst.max(finish);
            best = best.min(finish);
        });
        assert!(
            worst <= bound,
            "at {threads} rival thread(s) a single-threaded process took {worst} ns to spend \
             {WORK} ns of CPU, against a derived {bound} ns — two quanta of its own plus \
             {} dispatches of stale fair-band keys at {DISPATCH_NS} ns. Past this, forking \
             buys more CPU than the policy says it can.",
            threads + 2,
        );
        table.push((threads, worst, worst - 2 * WORK, WORK * 1000 / worst, best));
    }

    for &(threads, finish, deficit, permille, best) in &table {
        println!(
            "share_gain N={threads}: T_solo={finish} ns deficit={deficit} ns ({} quanta) \
             share={permille}permille (best seed {best})",
            deficit / QUANTUM_NS,
        );
    }

    // The measurement has to be a comparison and not an accident of a bound
    // nothing could reach: at one CPU the deficit is the band's granularity and
    // the scheduler spends nearly all of it.
    let (threads, _, deficit, _, _) = table[table.len() - 1];
    let bound = (threads as u64 + 2) * DISPATCH_NS;
    assert!(
        deficit * 2 > bound,
        "at {threads} rivals the deficit is {deficit} ns against a {bound} ns bound — more \
         than a factor of two of slack means the bound has stopped constraining anything and \
         a real regression could hide under it (it sits at 0.82 of the bound today)",
    );

    // And the shape of the finding, asserted rather than only tabulated: the
    // deficit grows with the rival count, which is what makes this a *bound* on
    // an attack rather than a constant nobody can move.
    let one = table[0].2;
    let many = table[table.len() - 1].2;
    assert!(
        one == 0 && many > 0,
        "the deficit went from {one} ns at one rival thread to {many} ns at {} — if it no \
         longer grows with the thread count, this case is measuring something else",
        table[table.len() - 1].0,
    );
}

/// **The deficit is an offset, not a drift** — which is the whole difference
/// between a granularity and a process that permanently owns more of the machine
/// than it is entitled to.
///
/// The case above measures `solo` finishing `N+1` quanta late over a 120 ms
/// window. That number is only tolerable if it is paid *once*: if it were paid
/// per unit of work, `swarm` would be taking a fixed fraction of the machine for
/// ever and the policy would simply be false. So the identical scenario is run
/// with `solo`'s work doubled and doubled again, and the deficit has to stay
/// where it is.
///
/// **What it found**, at 16 seeds per point:
///
/// | N | work | T_solo | deficit | share |
/// |---|---|---|---|---|
/// | 4  |  60 ms | 170 ms |  50 ms | 352‰ |
/// | 4  | 120 ms | 290 ms |  50 ms | 413‰ |
/// | 4  | 240 ms | 530 ms |  50 ms | 452‰ |
/// | 16 |  60 ms | 290 ms | 170 ms | 206‰ |
/// | 16 | 120 ms | 410 ms | 170 ms | 292‰ |
/// | 16 | 240 ms | 650 ms | 170 ms | 369‰ |
///
/// The deficit is the same nanosecond at every window length, so the share
/// climbs toward 500‰ as the window grows — the floor a reader wants is
/// `1/2 − (N+1) × QUANTUM / 2L` for a window of length `L`, and it is a floor
/// independent of N only in the limit. That sentence is the honest form of the
/// policy's claim, and this is the measurement it rests on.
#[test]
fn the_share_deficit_is_an_offset_and_not_a_drift() {
    for threads in [4usize, 16] {
        let mut deficits = Vec::new();
        for scale in [1u64, 2, 4] {
            let work = WORK * scale;
            let scenario = scenarios::share_gain(threads, work);
            let solo = scenario
                .process_index("solo")
                .expect("share_gain has a solo");
            let mut worst = 0;
            sweep(&scenario, SEEDS, |outcome| {
                worst =
                    worst.max(outcome.process_finish_ns[solo].expect("solo's work must complete"));
            });
            let deficit = worst - 2 * work;
            println!(
                "share_gain N={threads} work={work}: T_solo={worst} ns deficit={deficit} ns \
                 share={}permille",
                work * 1000 / worst,
            );
            deficits.push((work, worst, deficit));
        }
        let (_, _, first) = deficits[0];
        for &(work, finish, deficit) in &deficits[1..] {
            assert!(
                deficit <= first,
                "at {threads} rivals the deficit grew from {first} ns to {deficit} ns when the \
                 window went to {work} ns (T_solo {finish} ns). A deficit that scales with the \
                 work is not a granularity — it is a process permanently holding more of the \
                 machine than its share.",
            );
        }
        // And the corollary a reader actually wants: the share recovers toward
        // an even split as the window grows. Asserted, because "the deficit is
        // bounded" and "the split is fair over a long enough window" are the
        // same statement and only the second one is what a user feels.
        let share = |(work, finish, _): &(u64, u64, u64)| work * 1000 / finish;
        assert!(
            share(&deficits[2]) > share(&deficits[0]),
            "at {threads} rivals the share over a four-times-longer window is {}permille \
             against {}permille over the short one — it must climb, or the deficit is being \
             paid again",
            share(&deficits[2]),
            share(&deficits[0]),
        );
    }
}

/// The negative control for both cases above: spec §13.9's **rejected** policy,
/// one fair share per thread instead of one per process.
///
/// Without it, "a process cannot buy CPU by forking" is a number with nothing to
/// compare it to. Under per-thread shares `swarm` is entitled to N times what
/// `solo` is, and the instrument has to see that — not as a slightly worse
/// deficit, but as a different machine. Measured: `solo` takes **1,020 ms** to
/// spend the same 60 ms of CPU, against 290 ms under the shipped policy and a
/// 336 ms derived bound, so the case above would red by 3.0×.
///
/// **Invariant I5's ceiling is lifted for this run, and only for it.** Per-thread
/// shares fail I5 within a few hundred microseconds — that verdict is
/// `scenarios::fair_share_per_thread`'s and is asserted next door — and the
/// explorer stops at the first violation, so `solo` would never finish and the
/// quantity this file measures could not be read at all. The allowance suppresses
/// the *verdict* and changes no scheduling decision: `ShareShape::PerThread` is
/// still the whole of what differs from the control's control below.
#[test]
fn per_thread_shares_lose_the_floor() {
    const THREADS: usize = 16;
    let mut broken = scenarios::share_gain(THREADS, WORK).with_share(ShareShape::PerThread);
    // Past anything this run can produce, so I5 records the spread and reports
    // no violation; `Vm::fair_over_bound` still counts every crossing.
    broken = broken.with_fair_allowance(u64::MAX);
    let solo = broken.process_index("solo").expect("share_gain has a solo");
    let bound = 2 * WORK + (THREADS as u64 + 2) * DISPATCH_NS;

    let mut broken_finish = 0;
    sweep(&broken, SEEDS, |outcome| {
        broken_finish =
            broken_finish.max(outcome.process_finish_ns[solo].expect("solo's work must complete"));
    });

    let shipped = scenarios::share_gain(THREADS, WORK);
    let mut shipped_finish = 0;
    sweep(&shipped, SEEDS, |outcome| {
        shipped_finish =
            shipped_finish.max(outcome.process_finish_ns[solo].expect("solo's work must complete"));
    });

    println!(
        "per-thread control at {THREADS} rivals: T_solo={broken_finish} ns against \
         {shipped_finish} ns shipped and a {bound} ns bound",
    );
    assert!(
        broken_finish > bound,
        "under one fair share per *thread* a single-threaded process against {THREADS} rival \
         threads finished its 60 ms of work at {broken_finish} ns — inside the {bound} ns the \
         case above allows. A policy that hands `swarm` sixteen times `solo`'s entitlement has \
         to break that bound, or the bound is not measuring the policy.",
    );
    assert!(
        shipped_finish * 2 < broken_finish,
        "the shipped policy finished at {shipped_finish} ns and the rejected one at \
         {broken_finish} ns. Two shares that close together mean this control is detecting \
         the workload rather than the policy.",
    );
}

/// **Mixed interactive and background**: how long a thread that sleeps, wakes and
/// runs briefly waits for a CPU held by threads that never yield.
///
/// The claim under test is the kernel's own sentence, in
/// `mailbox::Urgency::Normal`: an ordinary wake is drained by a busy target "at
/// its next safe point (≤ one quantum)". The sleeper uses a quarter of a
/// millisecond every three, so it is far under its share, its stored lag is
/// positive and clamped, and `ShareState::enter_runnable` re-derives a vruntime
/// at `frontier − lag` — below every hog that has run recently. It should
/// therefore be picked the instant the running hog gives the CPU up.
///
/// **What it found**, 40 seeds per point, 20 wakes per run:
///
/// | cpus | hogs | wakes | worst wake | worst per-run runner-up | mean | derived bound |
/// |---|---|---|---|---|---|---|
/// | 1 |  1 | 700 |   8 ms |  7 ms |   0.36 ms |  36 ms |
/// | 1 |  4 | 440 |  37 ms |  8 ms |   2.20 ms |  72 ms |
/// | 1 | 16 | 360 | 157 ms |  0 ms |  14.74 ms | 216 ms |
/// | 1 | 64 | 180 | 637 ms |  0 ms | 136.16 ms | 792 ms |
/// | 2 |  4 | 414 |  21 ms |  8 ms |   0.78 ms |  48 ms |
/// | 2 | 16 | 389 |  86 ms | 30 ms |   5.67 ms | 120 ms |
///
/// Two things, and the second is the finding. **The one-quantum contract holds
/// for every wake but one, on one CPU** — the per-run runner-up never exceeds a
/// quantum plus the model's granularity, and the mean at one hog is a third of a
/// millisecond. And **the exception is the spawn burst**: at t=0 every hog thread
/// is queued at vruntime 0 with a key that stays stale until its own first
/// dispatch, so the sleeper's *first* wake waits behind all of them — 637 ms at
/// 64 hogs, which is 63.7 quanta for 64 rival threads. Every later wake finds a
/// band whose keys its lag beats, and is served at once.
///
/// So the bound asserted is the band's granularity — `(rivals + 1)` dispatches,
/// where rivals is what one run queue holds — and the sharper one-quantum claim
/// is asserted at one CPU on the runner-up, where the burst is provably a single
/// wake. At two CPUs the residue outlives one wake (30 ms) and that is recorded
/// here rather than asserted away.
#[test]
fn an_interactive_wake_waits_out_at_most_the_band_it_is_queued_behind() {
    /// Enough runs that the distribution has a few hundred wakes in it; the
    /// device interrupt's position among the enabled steps is the freedom the
    /// seeds explore.
    const WAKE_SEEDS: u64 = 40;
    for (cpus, hogs) in [(1usize, 1usize), (1, 4), (1, 16), (1, 64), (2, 4), (2, 16)] {
        let scenario = scenarios::interactive_mix(cpus, hogs);
        let sleeper = scenario
            .process_index("sleeper")
            .expect("interactive_mix has a sleeper");
        // What one run queue holds: the hogs spread over the CPUs, plus the
        // sleeper itself, plus the leader's extra quantum.
        let rivals = hogs.div_ceil(cpus) as u64 + 1;
        let bound = (rivals + 1) * DISPATCH_NS;

        let mut merged = Latency::default();
        let mut worst_runner_up = 0;
        sweep(&scenario, WAKE_SEEDS, |outcome| {
            let woken = outcome.wait(sleeper, ReadyCause::Woken);
            // Per *run*, because the claim is "every wake but one in a run" and
            // the union's second-largest across forty runs would be about all of
            // them together — see `Latency::merge`.
            worst_runner_up = worst_runner_up.max(woken.runner_up_ns());
            merged.merge(woken);
        });

        println!(
            "interactive cpus={cpus} hogs={hogs}: [{}] worst-run-2nd={worst_runner_up} ns \
             bound={bound} ns",
            merged.summary(),
        );
        assert!(
            merged.count() >= WAKE_SEEDS,
            "at {cpus} cpu(s) and {hogs} hog(s) the sleeper was woken {} time(s) over \
             {WAKE_SEEDS} runs — the distribution below is a measurement of nothing",
            merged.count(),
        );
        assert!(
            merged.max_ns() <= bound,
            "at {cpus} cpu(s) and {hogs} hog(s) an interactive wake waited {} ns against a \
             derived {bound} ns — {rivals} runnable thread(s) on one run queue plus the \
             leader, at {DISPATCH_NS} ns a dispatch. Distribution: {}",
            merged.max_ns(),
            merged.summary(),
        );
        if cpus == 1 {
            let contract = DISPATCH_NS;
            assert!(
                worst_runner_up <= contract,
                "at {hogs} hog(s) on one CPU, more than one wake per run missed the \
                 `Urgency::Normal` contract: the second-worst wake of some run took \
                 {worst_runner_up} ns against one quantum plus granularity, {contract} ns. \
                 The spawn burst delays the first wake and is measured as the maximum; a \
                 second slow wake means the fair band's keys are staying stale.",
            );
        }
    }
}

/// **Wakeup storms and the balance path**: many waiters made runnable at once,
/// over and over, on machines from one CPU to eight.
///
/// The drain — how long the last waiter waits for a CPU — is what a storm costs,
/// and the failure everyone fears is that it is *serialized*: `wake_all` claims
/// every waiter in one loop, and each claim posts a `Msg::Wake` to that waiter's
/// home CPU.
///
/// **What it found**, 20 seeds per point, 4 storms per run:
///
/// | cpus | waiters | wakes | worst drain | mean | migrations | derived bound |
/// |---|---|---|---|---|---|---|
/// | 1 | 16 |   820 |  9.00 ms |  3.80 ms |  0 | 204 ms |
/// | 2 | 16 |   743 | 18.75 ms |  3.49 ms |  4 | 108 ms |
/// | 4 | 16 |   656 | 16.25 ms |  3.51 ms |  3 |  60 ms |
/// | 1 | 64 | 3,460 | 27.75 ms | 11.37 ms |  0 | 780 ms |
/// | 4 | 64 | 2,938 | 41.75 ms | 10.50 ms | 33 | 204 ms |
/// | 8 | 64 | 2,697 | 57.00 ms | 11.35 ms | 43 | 108 ms |
///
/// **No pathological serialization**: quadrupling the storm from 16 to 64
/// waiters costs 3.1× the drain on one CPU, i.e. linear in the waiters a single
/// run queue holds, and the mean is a third of the worst at every width. What is
/// *not* there is any speed-up from a wider machine — the worst drain rises with
/// width rather than falling. It is well inside every derived bound, so it is
/// recorded as a finding rather than asserted as a defect: the mean is flat
/// across widths, so what the width costs is the tail, and a storm's tail on a
/// wide machine waits for the last busy CPU to reach a safe point rather than for
/// a queue to empty.
///
/// The balance path is exercised — 43 migrations at eight CPUs — which is what
/// makes this a statement about a machine that was rebalancing under the load
/// rather than about eight independent queues.
#[test]
fn a_wakeup_storm_drains_without_serializing() {
    const STORM_SEEDS: u64 = 20;
    let mut drains = Vec::new();
    for (cpus, waiters) in [
        (1usize, 16usize),
        (2, 16),
        (4, 16),
        (1, 64),
        (4, 64),
        (8, 64),
    ] {
        let scenario = scenarios::wakeup_storm(cpus, waiters);
        let index = scenario
            .process_index("waiters")
            .expect("wakeup_storm has waiters");
        let per_queue = waiters.div_ceil(cpus) as u64;
        let bound = (per_queue + 1) * DISPATCH_NS;

        let mut merged = Latency::default();
        let mut migrations = 0;
        sweep(&scenario, STORM_SEEDS, |outcome| {
            merged.merge(outcome.wait(index, ReadyCause::Woken));
            migrations = migrations.max(outcome.migrations);
        });

        println!(
            "storm cpus={cpus} waiters={waiters}: [{}] migrations={migrations} bound={bound} ns",
            merged.summary(),
        );
        assert!(
            merged.count() >= waiters as u64 * STORM_SEEDS,
            "a storm of {waiters} waiters over {STORM_SEEDS} runs produced only {} wake \
             latencies — most of the storm is not reaching the instrument",
            merged.count(),
        );
        assert!(
            merged.max_ns() <= bound,
            "at {cpus} cpu(s) the last of {waiters} woken waiters waited {} ns against a \
             derived {bound} ns — {per_queue} waiter(s) per run queue plus the leader, at \
             {DISPATCH_NS} ns a dispatch. Distribution: {}",
            merged.max_ns(),
            merged.summary(),
        );
        if cpus > 1 {
            assert!(
                migrations > 0,
                "at {cpus} cpus nothing was ever migrated, so this drain says nothing about \
                 a machine under a balance path — it is {cpus} independent queues",
            );
        }
        drains.push((cpus, waiters, merged.max_ns()));
    }

    // Linear in the waiters one queue holds, not quadratic in the storm. Both
    // points are at one CPU, so the whole storm lands in one run queue and the
    // comparison is about the drain and not about placement.
    let one_cpu = |waiters: usize| {
        drains
            .iter()
            .find(|&&(cpus, count, _)| cpus == 1 && count == waiters)
            .map(|&(_, _, drain)| drain)
            .expect("both one-CPU points were swept")
    };
    let (small, large) = (one_cpu(16), one_cpu(64));
    assert!(
        large <= small * 5,
        "quadrupling the storm from 16 to 64 waiters on one CPU took the drain from {small} \
         ns to {large} ns. Four times the waiters may cost four times the drain — that is what \
         one run queue serving them one at a time *is* — and this allows a quarter more on top; \
         past it the cost is growing faster than the queue and something outside the run queue \
         is serializing the storm.",
    );
    println!("storm drain 16→64 waiters on one cpu: {small} ns → {large} ns");
}

/// Seeds per point for the two adversarial-placement cases.
const LOPSIDED_SEEDS: u64 = 20;

/// The widths the adversarial machine is measured at: CPUs, and threads all
/// spawned onto cpu0 of them.
const LOPSIDED: [(usize, usize); 4] = [(2, 8), (4, 16), (4, 64), (8, 64)];

/// How far the balance path can drain the CPU an adversary loaded, derived.
///
/// Both halves of the pull path stop at the same inequality:
/// `answer_steal_requests` refuses below `fair_len() > 1` and `post_steal_probe`
/// will not probe a victim publishing a surplus under two — and `fair_len` is
/// sampled after the pick, so it excludes the task the CPU is about to run. So
/// the floor a loaded CPU can be drained to is one running task plus one queued
/// behind it, and everything above that floor is stealable, one task per
/// answered probe.
///
/// It is also the ceiling on the *whole run's* migrations, because in this
/// workload a thief never becomes a victim: a CPU holding one task publishes a
/// surplus of zero, so no task in flight here is ever moved twice.
fn stealable(threads: usize) -> u64 {
    threads as u64 - 2
}

/// **Adversarial placement**: every thread of a saturating workload spawned onto
/// one CPU, and what the balance path does with the machine that leaves.
///
/// Spawn placement is least-loaded-with-rotation, so this state arises from no
/// workload at all — which is why the simulator grew a placement knob for it
/// (`workload::PlacementShape`), and why until it did, the only balance path
/// anything had measured was the handful of probes a wakeup storm happens to
/// provoke. What this stages is the machine the pull half of §7.7 exists for:
/// every runnable thread on cpu0, every other CPU with nothing, and no wake, no
/// block and no second placement anywhere in the run.
///
/// **What it found**, 20 seeds per point at 60 ms of work per thread:
///
/// | cpus | threads | seeds reaching every CPU | seeds reaching a second CPU | best seed's migrations | the floor |
/// |---|---|---|---|---|---|
/// | 2 |  8 | 9/20 |  9/20 |  6 |  6 |
/// | 4 | 16 | 2/20 | 13/20 | 14 | 14 |
/// | 4 | 64 | 2/20 | 13/20 | 62 | 62 |
/// | 8 | 64 | 0/20 | 15/20 | 62 | 62 |
///
/// **Two findings, and the second is the one nobody had written down.**
///
/// Where the balance path runs at all it drains the loaded CPU *completely*:
/// the best seed at every width moves exactly [`stealable`] tasks — all but the
/// one cpu0 is running and the one queued behind it — so a machine an adversary
/// piled onto one CPU is emptied to the protocol's own floor, one task per
/// answered probe.
///
/// And **it only ever reaches the CPUs that are still awake.** The probe is
/// posted from an idle pass, one per idle trip, and only against a victim that
/// has already *published* a surplus of two; a CPU whose idle pass ran before
/// the loaded one published halts with no probe outstanding, and nothing in this
/// protocol wakes it — there is no push half, and a loaded CPU never announces
/// that it has surplus. That is why the first column falls with width: at eight
/// CPUs all seven have to lose the same race and none of the twenty seeds
/// did. `issues/kernel/an-idle-cpu-that-slept-before-the-surplus-is-never-probed.md`
/// carries it; the assertions below are about what the path does when it runs,
/// which is the half that is a law.
#[test]
fn the_balance_path_drains_the_cpu_an_adversary_loaded() {
    let mut table = Vec::new();
    for (cpus, threads) in LOPSIDED {
        let scenario = scenarios::lopsided_placement(cpus, threads, WORK);
        let floor = stealable(threads);
        let (mut best, mut widest, mut full, mut reached) = (0, 0, 0, 0);

        sweep(&scenario, LOPSIDED_SEEDS, |outcome| {
            let ran = outcome.first_exec_ns.iter().filter(|at| at.is_some()).count();
            // **Every CPU beyond cpu0 that ran was handed its first task by the
            // balance path**, because nothing else in this workload can put one
            // there: the placement is `AllOn(0)`, nothing blocks and nothing
            // wakes. An answered probe hands over exactly one task, so a machine
            // in which `ran` CPUs did work owes at least `ran - 1` migrations.
            assert!(
                outcome.migrations + 1 >= ran as u64,
                "at {cpus} cpus {ran} cpu(s) executed a step on a machine whose every thread \
                 was spawned onto cpu0, and the balance path moved only {} task(s) — one of \
                 those CPUs was given work by something that is not the balance path",
                outcome.migrations,
            );
            // And the ceiling: no task is moved twice, so the whole run cannot
            // exceed what cpu0 held above the surplus floor.
            assert!(
                outcome.migrations <= floor,
                "at {cpus} cpus the balance path moved {} of {threads} tasks, past the {floor} \
                 that stand above cpu0's surplus floor — a task is being migrated more than \
                 once, which is a thief that became a victim",
                outcome.migrations,
            );
            best = best.max(outcome.migrations);
            widest = widest.max(ran);
            full += usize::from(ran == cpus);
            reached += usize::from(ran > 1);
        });

        println!(
            "lopsided cpus={cpus} threads={threads}: {full}/{LOPSIDED_SEEDS} seeds reached \
             every cpu, {reached}/{LOPSIDED_SEEDS} reached a second, widest={widest}, best \
             seed moved {best} of a {floor} floor",
        );
        // **The drain, asserted where it is a law.** The best seed is the one in
        // which a thief was awake for the whole run, and there the balance path
        // has to empty cpu0 down to the floor and no further. Anything less is a
        // path that gives up while surplus is still published; anything more is
        // the ceiling above.
        assert_eq!(
            best, floor,
            "at {cpus} cpus and {threads} threads on cpu0, the best of {LOPSIDED_SEEDS} \
             schedules moved {best} tasks and the surplus floor leaves {floor} stealable. A \
             machine with a thief awake for the whole run must be drained to that floor: one \
             task per answered probe, until `fair_len() > 1` stops being true",
        );
        table.push((cpus, threads, full, reached, best, floor));
    }

    for &(cpus, threads, full, reached, best, floor) in &table {
        println!(
            "lopsided cpus={cpus} threads={threads}: every-cpu {full}/{LOPSIDED_SEEDS}, \
             second-cpu {reached}/{LOPSIDED_SEEDS}, drained {best}/{floor}",
        );
    }
}

/// The negative control for the case above: the same lopsided machine with the
/// pull half of the balance path switched off.
///
/// Without it "the balance path drains the CPU an adversary loaded" is a number
/// with nothing to compare it to — the simulator would report the same drain if
/// the tasks had been placed by the shipped policy in the first place, and the
/// case above would be measuring the placement knob rather than the balance
/// path. With `Env::steal` off nothing can move a task off cpu0 at all.
///
/// **Measured**: at every width the drain is 0 against a floor of 6, 14, 62 and
/// 62, and exactly one CPU of the machine ever executes a step — so the
/// assertion next door reds on its first width.
#[test]
fn without_the_balance_path_a_lopsided_machine_stays_lopsided() {
    for (cpus, threads) in LOPSIDED {
        let scenario =
            scenarios::lopsided_placement(cpus, threads, WORK).with_balance(Balance::None);
        let floor = stealable(threads);
        let (mut most, mut widest) = (0, 0);

        sweep(&scenario, LOPSIDED_SEEDS, |outcome| {
            most = most.max(outcome.migrations);
            widest = widest.max(outcome.first_exec_ns.iter().filter(|at| at.is_some()).count());
        });

        println!(
            "lopsided-control cpus={cpus} threads={threads}: {most} migrations against a \
             {floor} floor, {widest} of {cpus} cpus ran",
        );
        assert_eq!(
            most, 0,
            "with `Env::steal` off the balance path still moved {most} task(s) at {cpus} cpus \
             — nothing else in this protocol migrates, so this is not a control",
        );
        assert_eq!(
            widest, 1,
            "with the balance path off, {widest} of {cpus} cpus executed a step on a machine \
             whose every thread was spawned onto cpu0 — the placement knob is not staging the \
             machine the case above is about",
        );
    }
}

/// How long a cure waits before probing again, and how many times.
///
/// One quantum, because that is the interval the rest of this scheduler already
/// works in: a loaded CPU reaches a pass at every quantum boundary, so a thief
/// that re-probes faster than that is asking a question whose answer cannot have
/// changed. Four, because the re-arm has to be **bounded** — `times × every_ns`
/// is the whole window a cure of this shape can see a surplus in, and past it
/// the CPU halts for good rather than ticking for ever on a machine with nothing
/// to run.
const REARM_EVERY_NS: u64 = QUANTUM_NS;
const REARM_TIMES: u32 = 4;

/// The surplus a push fires at: `SchedPass::post_steal_probe`'s own inequality,
/// so a push never wakes a CPU whose victim would refuse it.
const PUSH_THRESHOLD: u32 = 2;

/// The four policies the two cure cases and the cost table are read across.
fn policies() -> [(&'static str, Balance); 4] {
    [
        ("pull (ships)", Balance::Pull),
        (
            "re-arm ×1",
            Balance::PullWithRearm {
                every_ns: REARM_EVERY_NS,
                times: 1,
            },
        ),
        (
            "re-arm ×4",
            Balance::PullWithRearm {
                every_ns: REARM_EVERY_NS,
                times: REARM_TIMES,
            },
        ),
        (
            "push ≥2",
            Balance::PushOnSurplus {
                threshold: PUSH_THRESHOLD,
            },
        ),
    ]
}

/// What one policy did with one lopsided machine, over a whole sweep.
#[derive(Default)]
struct Recovery {
    /// Seeds in which every CPU of the machine executed a step.
    full: usize,
    /// Seeds in which more than cpu0 did.
    second: usize,
    /// The most tasks the balance path moved in any seed.
    best_migrations: u64,
    /// The worst [`Outcome::probe_gap_ns`] any seed produced.
    gap_ns: u64,
    /// The worst and best `machine_working_at_ns` over the seeds that reached
    /// every CPU; `None` if no seed did.
    working_worst: Option<u64>,
    working_best: Option<u64>,
    /// Idle wakes summed over the sweep, and the worst single run's rate.
    wakes: u64,
    worst_rate: f64,
}

/// Wakes of a halted CPU that found nothing to do, per second of simulated time
/// — the unit a balance policy's cost is quoted in.
fn wake_rate(outcome: &Outcome) -> f64 {
    if outcome.elapsed == 0 {
        return 0.0;
    }
    outcome.idle_wakes_total() as f64 * 1e9 / outcome.elapsed as f64
}

/// Run the lopsided machine under one policy and fold what it did.
///
/// The two ceilings the shipped path already obeys are asserted here rather than
/// in each case, because they are claims about the *pull* half and every policy
/// below is built on it: no CPU is given work by anything but the balance path,
/// and no task is moved twice.
fn recover(cpus: usize, threads: usize, balance: Balance) -> Recovery {
    let scenario = scenarios::lopsided_placement(cpus, threads, WORK).with_balance(balance);
    let floor = stealable(threads);
    let mut r = Recovery::default();
    sweep(&scenario, LOPSIDED_SEEDS, |outcome| {
        let ran = outcome.cpus_reached();
        assert!(
            outcome.migrations + 1 >= ran as u64,
            "{balance:?} at {cpus} cpus: {ran} cpu(s) executed a step on a machine whose every \
             thread was spawned onto cpu0, and the balance path moved only {} task(s) — one of \
             those CPUs was given work by something that is not the balance path",
            outcome.migrations,
        );
        assert!(
            outcome.migrations <= floor,
            "{balance:?} at {cpus} cpus: the balance path moved {} of {threads} tasks, past the \
             {floor} that stand above cpu0's surplus floor — a task is being migrated more than \
             once, which is a thief that became a victim",
            outcome.migrations,
        );
        r.full += usize::from(ran == cpus);
        r.second += usize::from(ran > 1);
        r.best_migrations = r.best_migrations.max(outcome.migrations);
        r.gap_ns = r.gap_ns.max(outcome.probe_gap_ns);
        r.wakes += outcome.idle_wakes_total();
        r.worst_rate = r.worst_rate.max(wake_rate(outcome));
        if let Some(at) = outcome.machine_working_at_ns() {
            r.working_worst = Some(r.working_worst.unwrap_or(0).max(at));
            r.working_best = Some(r.working_best.unwrap_or(u64::MAX).min(at));
        }
    });
    r
}

/// The condition under which "every CPU is reached" is a statement about the
/// balance path rather than about arithmetic: cpu0 can be drained to
/// [`stealable`] and every other CPU needs one task, so there has to be at least
/// one task per starved CPU above the floor.
fn enough_to_go_round(cpus: usize, threads: usize) {
    assert!(
        stealable(threads) >= cpus as u64 - 1,
        "at {cpus} cpus and {threads} threads only {} task(s) stand above cpu0's surplus floor \
         and {} CPU(s) need one each — no balance policy can reach every CPU here, so a case \
         asserting that it does would be asserting arithmetic",
        stealable(threads),
        cpus - 1,
    );
}

/// **Cure one, measured: a bounded re-arm of the probe.**
///
/// `issues/kernel/an-idle-cpu-that-slept-before-the-surplus-is-never-probed.md`
/// names two ways out of the one-shot probe, and this is the one that needs no
/// observation of anything: a CPU that halts with nothing to run programs its
/// one-shot timer [`REARM_EVERY_NS`] ahead and probes again when it fires, up to
/// [`REARM_TIMES`] times per idle period. Nothing has to notice it, nothing has
/// to publish anything, and the timer fires whether or not a surplus ever
/// appeared — which is both why it works and what it costs.
///
/// **The derivation, and it is the assertion.** A CPU that halted at `H` is
/// woken at `H + every_ns`: the model's step relation forbids an execution step
/// anywhere while a CPU owes a timer delivery, so the clock cannot run past the
/// armed instant unpunished. Three [`RUN_CHUNK_NS`] chunks of granularity sit on
/// top, and each is a step the model permits before the probe is posted — the
/// execution step that carries the clock over the armed instant, the one chunk
/// of grace `Vm::enabled` gives a CPU that owes a rescheduling pass, and the
/// execution step that carries the clock over *that*. So no CPU may sit halted
/// beside a published surplus with no probe outstanding for longer than
/// `every_ns + 3 × RUN_CHUNK` — [`Outcome::probe_gap_ns`] is that quantity, and
/// the surplus in this workload is published at clock 0, before any CPU can have
/// halted, so one re-arm is inside the window at every width.
///
/// **What it found**, 20 seeds per width at 60 ms of work per thread and
/// `every_ns = QUANTUM_NS`:
///
/// | cpus | threads | every CPU, ×1 | every CPU, ×4 | pull | probe gap | pull's gap | idle wakes/s, worst run (×1 / ×4) |
/// |---|---|---|---|---|---|---|---|
/// | 2 |  8 | 20/20 | 20/20 | 9/20 | 10.0 ms |   480 ms |  8.16 / 21.15 |
/// | 4 | 16 | 20/20 | 20/20 | 2/20 | 10.0 ms |   960 ms |  8.25 / 23.00 |
/// | 4 | 64 | 20/20 | 20/20 | 2/20 | 10.0 ms | 3,840 ms |  3.38 /  6.44 |
/// | 8 | 64 | 20/20 | 20/20 | 0/20 | 10.0 ms | 3,840 ms |  6.75 / 12.89 |
///
/// The derived bound is 13,000,000 ns and the measurement is 10,000,000 ns at
/// every width — 0.77 of it, which is where the rest of this file's numbers sit.
/// It is *exactly* `every_ns`, because this workload's clock is a multiple of
/// `RUN_CHUNK_NS` throughout and none of the three granularity chunks is ever
/// spent; they stay in the bound because the model permits them, not because a
/// schedule was found that needs them. `Balance::Pull`'s own gap is the whole
/// run at every width, because nothing in that protocol can close it.
///
/// **One re-arm is enough here**: the ×1 and ×4 columns are the same recovery
/// for two and a half times the wakes. What this workload needs is *a* second
/// look, not a periodic one — and `times` is what decides how much of the second
/// kind is bought with it.
///
/// **The negative control.** With the cure off and everything else identical
/// (`Balance::Pull` substituted for the policy under test) this case reds on its
/// first width, at the recovery assertion:
///
/// ```text
/// with a re-arm every 10000000 ns, 11 of 20 schedules left a CPU of 2 asleep on
/// a machine whose whole workload sits on cpu0. [...]  left: 9  right: 20
/// ```
///
/// and with that assertion elided, at the derived timing one behind it:
///
/// ```text
/// a CPU sat halted beside a published surplus with no probe outstanding for
/// 480000000 ns, against a derived 13000000 ns
/// ```
///
/// A CPU's first execution step is reported and **not** asserted: the model does
/// not oblige a CPU that has a task loaded to take one, so `working_at` measures
/// the explorer's freedom as much as the protocol's recovery — 20,000,000 ns in
/// the best seed at eight CPUs against 3,780,000,000 ns in the worst, on runs
/// whose probe gap is 10,000,000 ns either way. That is the same limit the case
/// above states about makespan, and it is why recovery is counted in CPUs and
/// timed in probe gaps.
#[test]
fn a_bounded_re_arm_reaches_every_cpu_the_pull_path_left_asleep() {
    let bound = REARM_EVERY_NS + 3 * RUN_CHUNK_NS;
    let mut table = Vec::new();
    for (cpus, threads) in LOPSIDED {
        enough_to_go_round(cpus, threads);
        let floor = stealable(threads);
        let control = recover(cpus, threads, Balance::Pull);
        for times in [1, REARM_TIMES] {
            let balance = Balance::PullWithRearm {
                every_ns: REARM_EVERY_NS,
                times,
            };
            let cured = recover(cpus, threads, balance);
            println!(
                "re-arm ×{times} cpus={cpus} threads={threads}: every-cpu \
                 {}/{LOPSIDED_SEEDS} (pull {}/{LOPSIDED_SEEDS}), gap={} ns (pull {} ns, bound \
                 {bound} ns), drained {}/{floor}, working_at {:?}..{:?} ns, {} idle wakes over \
                 the sweep, worst run {:.2}/s",
                cured.full,
                control.full,
                cured.gap_ns,
                control.gap_ns,
                cured.best_migrations,
                cured.working_best,
                cured.working_worst,
                cured.wakes,
                cured.worst_rate,
            );

            // **The law.** A cure that re-probes every `every_ns` cannot leave a
            // CPU asleep beside a surplus that was published before it halted,
            // and in this workload every surplus is.
            assert_eq!(
                cured.full, LOPSIDED_SEEDS as usize,
                "with a re-arm every {REARM_EVERY_NS} ns, {} of {LOPSIDED_SEEDS} schedules left \
                 a CPU of {cpus} asleep on a machine whose whole workload sits on cpu0. cpu0 \
                 publishes its surplus at clock 0 — before any CPU can have halted, since no \
                 execution step is enabled until it has taken its first pass — so every halt is \
                 inside the first re-arm's window and every CPU must be probed.",
                LOPSIDED_SEEDS as usize - cured.full,
            );
            assert!(
                cured.gap_ns <= bound,
                "a CPU sat halted beside a published surplus with no probe outstanding for {} \
                 ns, against a derived {bound} ns — one re-arm period at {REARM_EVERY_NS} ns \
                 plus three {RUN_CHUNK_NS} ns chunks of the model's own granularity. Past this \
                 the timer is not what is waking the CPU.",
                cured.gap_ns,
            );
            // And the drain is unchanged: a cure that reaches every CPU by
            // moving more tasks than cpu0 has to give would be a different
            // machine, not a repaired one.
            assert_eq!(
                cured.best_migrations, floor,
                "the best of {LOPSIDED_SEEDS} schedules moved {} tasks and the surplus floor \
                 leaves {floor} stealable — the re-arm changes when a probe is posted and \
                 nothing about what answering one hands over",
                cured.best_migrations,
            );
            table.push((cpus, threads, times, cured, control.full, control.gap_ns));
        }
    }

    // The negative control, stated where the comparison is: with the cure off,
    // the same assertion has to fail. It does — at eight CPUs not one of the
    // twenty schedules reaches every CPU.
    let (_, _, _, _, worst_control_full, _) = table
        .iter()
        .map(|&(c, t, times, ref cured, full, gap)| (c, t, times, cured.full, full, gap))
        .min_by_key(|&(_, _, _, _, full, _)| full)
        .expect("the sweep ran");
    assert!(
        worst_control_full < LOPSIDED_SEEDS as usize,
        "`Balance::Pull` reached every CPU in every one of {LOPSIDED_SEEDS} schedules at every \
         width — then the case above is not measuring a cure, because there is nothing left to \
         cure",
    );
}

/// **Cure two, measured: a push on surplus.**
///
/// The other way out of the one-shot probe, and the one that costs almost
/// nothing: a pass that publishes a surplus of [`PUSH_THRESHOLD`] or more rings
/// the doorbell of one CPU that reads SLEEPING. No task moves on that ring — the
/// woken CPU runs its own idle pass and posts an ordinary probe — so the push
/// adds no second way to migrate anything, only a way to make the pull half run
/// on a CPU that had stopped asking.
///
/// **The derivation, and it is the assertion.** A pass pushes to one CPU, and
/// `SchedPass::push_on_surplus`'s cursor makes consecutive pushes walk the
/// machine, so `k` sleeping CPUs are reached in `k` passes of the CPU holding the
/// surplus. That CPU is running a task, so it reaches a pass at each quantum
/// boundary: `k × DISPATCH_NS`. The kick is an IPI ([`IPI_LATENCY_NS`], which the
/// model enforces by disabling every execution step while a delivery is overdue)
/// and the pass it asks for is one chunk of grace plus the chunk that carries the
/// clock over it. Here `k` is `cpus − 1`, every CPU but the loaded one, so
///
/// ```text
/// (cpus − 1) × DISPATCH_NS + IPI_LATENCY_NS + 2 × RUN_CHUNK_NS
/// ```
///
/// **What it found**, 20 seeds per width:
///
/// | cpus | threads | seeds reaching every CPU | pull | probe gap | derived | measured/bound | idle wakes/s, worst run |
/// |---|---|---|---|---|---|---|---|
/// | 2 |  8 | 20/20 | 9/20 |  1.0 ms | 14.2 ms | 0.07 | 2.08 |
/// | 4 | 16 | 20/20 | 2/20 | 22.0 ms | 38.2 ms | 0.58 | 3.12 |
/// | 4 | 64 | 20/20 | 2/20 | 22.0 ms | 38.2 ms | 0.58 | 0.78 |
/// | 8 | 64 | 20/20 | 0/20 | 66.0 ms | 86.2 ms | 0.77 | 1.82 |
///
/// The bound is loose at two CPUs for the reason it is tight at eight: there is
/// one sleeper to reach and the first pass reaches it, so `k = 1` and the `k`
/// term has nothing to say. The wakes it costs are 11, 36, 36 and 74 over a
/// twenty-seed sweep — between three and eleven times fewer than the cheapest
/// re-arm buys the same recovery for.
///
/// **The cursor is load-bearing and the measurement is what said so.** Without
/// it every push goes to the lowest-numbered sleeper, which posts its probe and
/// halts again with SLEEPING still set — so the next pass re-pokes the CPU that
/// is already coming, and the one behind it waits for the first one's probe to
/// be *answered*. Two passes per sleeper: 130,000,000 ns of probe gap at eight
/// CPUs against the 66,000,000 ns above.
///
/// **The negative control**, `Balance::Pull` substituted for the policy under
/// test and nothing else changed — the recovery assertion first:
///
/// ```text
/// with a push at a surplus of 2, 11 of 20 schedules left a CPU of 2 asleep on a
/// machine whose whole workload sits on cpu0. [...]  left: 9  right: 20
/// ```
///
/// and the derived timing one behind it:
///
/// ```text
/// a CPU sat halted beside a published surplus with no probe outstanding for
/// 480000000 ns, against a derived 14200000 ns
/// ```
#[test]
fn a_push_on_surplus_reaches_every_cpu_the_pull_path_left_asleep() {
    let balance = Balance::PushOnSurplus {
        threshold: PUSH_THRESHOLD,
    };
    let mut reached_all = 0;
    for (cpus, threads) in LOPSIDED {
        enough_to_go_round(cpus, threads);
        let floor = stealable(threads);
        let sleepers = cpus as u64 - 1;
        let bound = sleepers * DISPATCH_NS + IPI_LATENCY_NS + 2 * RUN_CHUNK_NS;
        let control = recover(cpus, threads, Balance::Pull);
        let cured = recover(cpus, threads, balance);

        println!(
            "push ≥{PUSH_THRESHOLD} cpus={cpus} threads={threads}: every-cpu \
             {}/{LOPSIDED_SEEDS} (pull {}/{LOPSIDED_SEEDS}), gap={} ns (pull {} ns, bound \
             {bound} ns), drained {}/{floor}, working_at {:?}..{:?} ns, {} idle wakes over the \
             sweep, worst run {:.2}/s",
            cured.full,
            control.full,
            cured.gap_ns,
            control.gap_ns,
            cured.best_migrations,
            cured.working_best,
            cured.working_worst,
            cured.wakes,
            cured.worst_rate,
        );

        assert_eq!(
            cured.full, LOPSIDED_SEEDS as usize,
            "with a push at a surplus of {PUSH_THRESHOLD}, {} of {LOPSIDED_SEEDS} schedules \
             left a CPU of {cpus} asleep on a machine whose whole workload sits on cpu0. cpu0 \
             publishes a surplus of {} at clock 0 and pushes to one sleeping CPU per pass, \
             walking them in turn, so every one of them is rung inside {sleepers} passes.",
            LOPSIDED_SEEDS as usize - cured.full,
            threads - 1,
        );
        assert!(
            cured.gap_ns <= bound,
            "a CPU sat halted beside a published surplus with no probe outstanding for {} ns, \
             against a derived {bound} ns — {sleepers} sleeping CPU(s) at one push per pass and \
             {DISPATCH_NS} ns a pass, plus {IPI_LATENCY_NS} ns of IPI latency and two \
             {RUN_CHUNK_NS} ns chunks for the pass the kick asks for. Past this the push is \
             reaching the same CPU twice instead of walking the machine.",
            cured.gap_ns,
        );
        assert_eq!(
            cured.best_migrations, floor,
            "the best of {LOPSIDED_SEEDS} schedules moved {} tasks and the surplus floor leaves \
             {floor} stealable — the push changes which CPU asks and nothing about what \
             answering hands over",
            cured.best_migrations,
        );
        reached_all += usize::from(control.full == LOPSIDED_SEEDS as usize);
    }
    assert!(
        reached_all < LOPSIDED.len(),
        "`Balance::Pull` reached every CPU in every schedule at every width — then the case \
         above is not measuring a cure",
    );
}

/// **What the two cures cost the idle path**, on the workloads a desktop
/// actually runs.
///
/// `kernel/CLAUDE.md` makes anything added to the idle loop an audio change, and
/// both cures add exactly one kind of thing: a wake of a CPU that had halted.
/// [`Outcome::idle_wakes`] counts them — a wake whose first pass reaches the idle
/// disposition again — so the count is a property of the run rather than of the
/// policy, and `Balance::Pull`'s own figure is the baseline the others are read
/// against. It is **zero on every workload here**, which is what makes the
/// comparison one.
///
/// **What it found**, 20 seeds per point. The first figure is the whole sweep's
/// wakes, the second the worst single run's rate against simulated time:
///
/// | workload | pull | re-arm ×1 | re-arm ×4 | push ≥2 |
/// |---|---|---|---|---|
/// | `interactive_mix(2,4)`  | 0, 0.00/s |  44,  31.58/s |  171,  88.00/s |    9,  11.76/s |
/// | `interactive_mix(2,16)` | 0, 0.00/s |  49,  11.94/s |  174,  30.14/s |    2,   3.08/s |
/// | `interactive_mix(4,16)` | 0, 0.00/s | 104,  20.90/s |  363,  63.01/s |   53,  24.62/s |
/// | `wakeup_storm(4,16)`    | 0, 0.00/s | 251,  92.13/s | 1004, 306.22/s |   80,  97.56/s |
/// | `wakeup_storm(8,64)`    | 0, 0.00/s | 521, 150.67/s | 2075, 534.78/s |  748, 380.95/s |
/// | `audio_pipeline(4)`     | 0, 0.00/s | 111, 153.85/s |  351, 243.90/s | **0, 0.00/s** |
///
/// **The last row is the decision.** `audio_pipeline` is four threads on four
/// CPUs, so no CPU ever holds a fair band two deep and no CPU ever has surplus to
/// announce: the push fires **not once** in the whole sweep and costs the idle
/// path literally nothing, while the re-arm ticks every idle CPU regardless,
/// because a timer cannot ask whether there is anything to come for. That is the
/// difference between the two cures stated in the unit the owner has to decide
/// in, on the workload the owner cares about.
///
/// **And the row above it is the qualification.** Under a wakeup storm at eight
/// CPUs the push costs 748 wakes against the cheapest re-arm's 521: a storm is
/// a machine that keeps producing surplus beside CPUs that keep going idle, so
/// the observation the push rests on keeps coming back true. The push is cheap
/// where nothing is queued and dear where a great deal is — which is the
/// opposite way round from the re-arm, and the reason the two rows are both here.
///
/// **The wake latency does not move**, which is the other half of the same
/// question. The watched thread's worst wake is the same nanosecond under all
/// four policies at every point but one (`wakeup_storm(8,64)`, where the push's
/// 50,750,000 ns is *better* than the shipped path's 57,000,000 ns), and the mean
/// moves by under 2%. That is a schedule perturbation and not a price: the model
/// charges a scheduler pass zero nanoseconds (`SimHwState::pass_cost_ns`), so it
/// can *count* an extra wake and cannot bill one. The count is what goes to the
/// owner; what a wake costs the CPU it wakes is the kernel's own measurement to
/// make.
#[test]
fn the_two_cures_are_priced_against_the_pull_path() {
    const COST_SEEDS: u64 = 20;
    let points: [(&str, usize, usize, Scenario); 6] = [
        ("interactive_mix(2,4)", 2, 4, scenarios::interactive_mix(2, 4)),
        ("interactive_mix(2,16)", 2, 16, scenarios::interactive_mix(2, 16)),
        ("interactive_mix(4,16)", 4, 16, scenarios::interactive_mix(4, 16)),
        ("wakeup_storm(4,16)", 4, 16, scenarios::wakeup_storm(4, 16)),
        ("wakeup_storm(8,64)", 8, 64, scenarios::wakeup_storm(8, 64)),
        ("audio_pipeline(4)", 4, 0, scenarios::audio_pipeline(4)),
    ];
    for (label, cpus, hogs, scenario) in points {
        // Whose wake latency this workload is about: the interactive thread, the
        // storm's waiters, or the audio clients.
        let watched = scenario
            .procs
            .iter()
            .position(|p| matches!(p.name, "sleeper" | "waiters" | "client"))
            .expect("every workload here has a thread whose wake latency is the point");
        // One run queue's worth of rivals plus the leader, exactly as
        // `an_interactive_wake_waits_out_at_most_the_band_it_is_queued_behind`
        // derives it. `audio_pipeline` carries its own thread count rather than
        // a hog parameter, so it is counted from the scenario.
        let rivals = if hogs > 0 {
            hogs.div_ceil(cpus) as u64 + 1
        } else {
            scenario.procs.iter().map(|p| p.initial.len() as u64).sum::<u64>() / cpus as u64 + 1
        };
        let bound = (rivals + 1) * DISPATCH_NS;

        for (name, balance) in policies() {
            let scenario = scenario.clone().with_balance(balance);
            let mut woken = Latency::default();
            let (mut wakes, mut worst_rate) = (0u64, 0.0f64);
            sweep(&scenario, COST_SEEDS, |outcome| {
                woken.merge(outcome.wait(watched, ReadyCause::Woken));
                wakes += outcome.idle_wakes_total();
                worst_rate = worst_rate.max(wake_rate(outcome));
            });
            println!(
                "cost {label} [{name}]: idle wakes {wakes} over {COST_SEEDS} runs, worst run \
                 {worst_rate:.2}/s; woken[{}] bound={bound} ns",
                woken.summary(),
            );

            assert!(
                woken.count() >= COST_SEEDS,
                "{label} [{name}]: the watched thread was woken {} time(s) over {COST_SEEDS} \
                 runs — the distribution below is a measurement of nothing",
                woken.count(),
            );
            // The audio-relevant claim, asserted under every policy and not only
            // the shipped one: a cure that lengthened the wake path would be
            // paying for the balance path out of the interactive one.
            assert!(
                woken.max_ns() <= bound,
                "{label} [{name}]: a wake waited {} ns against a derived {bound} ns — {rivals} \
                 runnable thread(s) on one run queue plus the leader, at {DISPATCH_NS} ns a \
                 dispatch. A balance policy may cost idle wakes; it may not cost wake latency. \
                 Distribution: {}",
                woken.max_ns(),
                woken.summary(),
            );
            // And the baseline is a baseline. Under `Pull` nothing arms a timer
            // on a CPU with an empty queue and nothing rings a doorbell without
            // a message behind it, so every wake of a halted CPU here carries
            // work — if this stops being zero the column the cures are read
            // against has stopped meaning "what the shipped path costs".
            if balance == Balance::Pull {
                assert_eq!(
                    wakes, 0,
                    "{label}: the shipped balance path woke a halted CPU {wakes} time(s) for \
                     nothing over {COST_SEEDS} runs. Every column beside it is quoted as a cost \
                     *over* this one",
                );
            }
        }
    }
}

/// **The starvation bound**: the worst wait of a runnable task under saturation,
/// measured, against the fair band's own granularity.
///
/// This is the same quantity the two cases above measure from their own ends —
/// an interactive wake and a storm drain are both a task waiting for a run queue
/// to reach it — asked here of *every* task in a workload where nothing ever
/// blocks and no CPU is ever idle. There is nothing to wait for but the band.
///
/// **What it found**, 20 seeds per point:
///
/// | workload | runnable threads | waits seen | worst wait | in quanta | derived bound | ratio |
/// |---|---|---|---|---|---|---|
/// | `fairness_storm(1)` |  4 |   780 |  50 ms |  5 |  60 ms | 0.83 |
/// | `sibling_storm`     |  4 | 3,660 |  50 ms |  5 |  60 ms | 0.83 |
/// | `share_gain(4)`     |  5 |   680 |  70 ms |  7 |  72 ms | 0.97 |
/// | `share_gain(16)`    | 17 | 2,360 | 200 ms | 20 | 216 ms | 0.93 |
/// | `share_gain(64)`    | 65 | 9,080 | 680 ms | 68 | 792 ms | 0.86 |
///
/// The bound is `(runnable threads + 1) × (QUANTUM + 2 × RUN_CHUNK)`, which is
/// invariant I13's bound with a different name on it, and the measurement is at
/// 0.83–0.97 of it everywhere. **Nothing starves, and the price of that sentence
/// is a linear one**: a task's worst wait is one dispatch per runnable thread on
/// its CPU, so a machine carrying 65 runnable threads makes some task wait 680
/// ms. That is what "no starvation" is worth here, as a number.
#[test]
fn a_runnable_task_waits_at_most_one_dispatch_per_rival() {
    const STARVE_SEEDS: u64 = 20;
    for scenario in [
        scenarios::fairness_storm(1),
        scenarios::sibling_storm(),
        scenarios::share_gain(4, WORK),
        scenarios::share_gain(16, WORK),
        scenarios::share_gain(64, WORK),
    ] {
        let name = scenario.name;
        // Every thread of these workloads is runnable from the first dispatch to
        // its own exit, and every one of them is on the one CPU.
        let threads: u64 = scenario.procs.iter().map(|p| p.initial.len() as u64).sum();
        assert_eq!(
            scenario.cpus, 1,
            "{name}: the bound below is one run queue's"
        );
        let bound = (threads + 1) * DISPATCH_NS;

        let mut worst = 0;
        let mut samples = 0;
        sweep(&scenario, STARVE_SEEDS, |outcome| {
            worst = worst.max(outcome.worst_run_wait_ns());
            samples += outcome
                .run_wait
                .iter()
                .map(toyos_sched_sim::latency::RunWait::samples)
                .sum::<u64>();
        });
        println!(
            "starvation {name} ({threads} runnable threads): worst={worst} ns \
             ({} quanta) bound={bound} ns over {samples} waits",
            worst / QUANTUM_NS,
        );
        assert!(
            samples > threads * STARVE_SEEDS,
            "{name}: only {samples} run-queue waits were recorded over {STARVE_SEEDS} runs of \
             {threads} threads, so this bound is being met by an instrument that stopped \
             measuring",
        );
        assert!(
            worst <= bound,
            "{name}: a runnable task waited {worst} ns for a CPU against a derived {bound} ns \
             — one dispatch per runnable thread on its run queue plus the leader's, at \
             {DISPATCH_NS} ns each. Past this, the fair band is passing some task over more \
             times than its insertion-time keys can account for.",
        );
        // Tight enough to constrain: the shipped scheduler sits at 0.83–0.97 of
        // this everywhere, so a factor of two of slack would already be a bound
        // that had stopped measuring.
        assert!(
            worst * 2 > bound,
            "{name}: the worst wait is {worst} ns against a {bound} ns bound, more than twice \
             under it — the bound has stopped constraining anything",
        );
    }
}
