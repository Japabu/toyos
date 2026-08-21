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

use toyos_sched::fair::QUANTUM_NS;
use toyos_sched_sim::choice::ChoiceStream;
use toyos_sched_sim::explore::{run, Outcome};
use toyos_sched_sim::latency::{Latency, ReadyCause};
use toyos_sched_sim::scenarios;
use toyos_sched_sim::vm::RUN_CHUNK_NS;
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
