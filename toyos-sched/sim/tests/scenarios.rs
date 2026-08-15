//! The Stage 4 gate (spec §11).
//!
//! The exit criterion is two claims, and the second is worth nothing without
//! the first:
//!
//! 1. **`old_steal_port` fails.** A deliberate port of the OLD scheduler's
//!    steal-and-scan algorithm must be caught. A harness that has never
//!    rejected the bug class it was written for is decoration, so this test
//!    asserts the failure, and asserts it is the *right* failure.
//! 2. **Every scenario passes**, over a seed sweep and a fuzz-byte sweep.
//!
//! The budgets here are what a `cargo test` can afford. The full criterion —
//! 10⁴ seeds and 10⁷ fuzz steps per scenario class — runs from the CLI:
//!
//! ```text
//! cargo run --release -p toyos-sched-sim -- gate 10000
//! cargo run --release -p toyos-sched-sim -- fuzz-sweep 10000000
//! ```

use std::collections::BTreeMap;

use toyos_sched_sim::choice::ChoiceStream;
use toyos_sched_sim::explore::run;
use toyos_sched_sim::scenarios;
use toyos_sched_sim::shrink;
use toyos_sched_sim::sweep;
use toyos_sched_sim::workload::ParkShape;

/// Seeds per scenario in the in-test sweep. Every seed is a complete
/// exploration with all of I1–I13 checked after every step.
const SEEDS: u64 = 500;

/// Steps per scenario in the in-test fuzz sweep.
const FUZZ_STEPS: u64 = 20_000;

/// Seeds for the fairness gates and their controls. A fifth of [`SEEDS`],
/// because `fairness_storm` is by far the longest scenario in the tree — it has
/// to be, since a contention window must be many quanta wide before a broken
/// split can clear the bound — and `every_scenario_survives_a_seed_sweep`
/// already runs it at the full count. What these tests add is the *direction* of
/// each failure, and a structural 3:1 split does not need 500 schedules to show
/// up in.
const FAIR_SEEDS: u64 = 100;

#[test]
fn every_scenario_survives_a_seed_sweep() {
    let mut failures = Vec::new();
    let mut total = 0u64;
    for scenario in scenarios::all() {
        let result = sweep::seed_sweep(&scenario, SEEDS, 3);
        total += result.steps;
        if !result.passed() {
            failures.push(result.report());
        }
    }
    assert!(
        failures.is_empty(),
        "seed sweep found violations:\n{}",
        failures.join("\n"),
    );
    assert!(
        total > 100_000,
        "the sweep must actually explore: {total} steps"
    );
}

#[test]
fn every_scenario_survives_raw_fuzz_bytes() {
    let mut failures = Vec::new();
    for scenario in scenarios::all() {
        let result = sweep::fuzz_sweep(&scenario, FUZZ_STEPS, 3);
        if !result.passed() {
            failures.push(result.report());
        }
    }
    assert!(
        failures.is_empty(),
        "fuzz sweep found violations:\n{}",
        failures.join("\n"),
    );
}

/// The self-validation gate. Both failure modes the old algorithm has are
/// required to show up, because they are different bugs wearing one name:
///
/// * **I1** — the task is in no container at all (carried on the thief's
///   stack) or in a queue its state word does not name. Single ownership,
///   lost.
/// * **I8** — the teardown drew a proof of absence against a task that was
///   merely in transit, and freed the address space that task still holds.
///   That is the crash.md failure itself.
#[test]
fn old_steal_port_is_caught() {
    let scenario = scenarios::old_steal_port();
    let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
    let mut caught = 0;
    for seed in 0..SEEDS {
        let mut choices = if seed % 2 == 0 {
            ChoiceStream::from_seed(seed)
        } else {
            ChoiceStream::pct(seed, scenario.cpus, 3)
        };
        let outcome = run(scenario.clone(), &mut choices);
        if outcome.passed() {
            continue;
        }
        caught += 1;
        for violation in &outcome.violations {
            let id = violation.split(':').next().unwrap_or("?").to_string();
            *kinds.entry(id).or_default() += 1;
        }
    }
    assert!(
        caught > 0,
        "the old steal-and-scan protocol went undetected in {SEEDS} schedules — \
         nothing this harness says about the new one means anything",
    );
    assert!(
        kinds.contains_key("I1"),
        "expected a single-ownership violation; got {kinds:?}",
    );
    assert!(
        kinds.contains_key("I8"),
        "expected the address-space-freed-while-referenced violation \
         (the crash.md shape itself); got {kinds:?}",
    );
}

/// The second self-validation gate: the kernel's pre-`8508b37` blocking shape,
/// where phase 2 of the §8.1 handshake ran at the call site instead of inside
/// the blocking pass. On `--smp 8` that was a panic plus a 30 s hang in roughly
/// two of five audio suite runs.
///
/// It must be caught, and caught by **I1** specifically: a task whose word says
/// `Blocked` while its own CPU still has it in `running` is a single-ownership
/// break, and it is the break that makes the lost wake possible — the waker
/// reads `Blocked`, posts to the home CPU, and that CPU's own pass drains the
/// message before the task is anywhere a wake can find it.
#[test]
fn old_commit_before_pass_is_caught() {
    let scenario = scenarios::old_commit_before_pass();
    let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
    let mut caught = 0;
    let mut worst_steps = 0;
    for seed in 0..SEEDS {
        let mut choices = if seed % 2 == 0 {
            ChoiceStream::from_seed(seed)
        } else {
            ChoiceStream::pct(seed, scenario.cpus, 3)
        };
        let outcome = run(scenario.clone(), &mut choices);
        if outcome.passed() {
            continue;
        }
        caught += 1;
        worst_steps = worst_steps.max(outcome.steps);
        for violation in &outcome.violations {
            let id = violation.split(':').next().unwrap_or("?").to_string();
            *kinds.entry(id).or_default() += 1;
        }
    }
    assert_eq!(
        caught, SEEDS as usize,
        "committing the wait ticket before the pass must be caught in every \
         schedule, not merely in some: {caught}/{SEEDS}",
    );
    assert!(
        worst_steps < 32,
        "the worst schedule took {worst_steps} steps to expose it; this is a \
         structural break and should surface almost immediately",
    );
    assert!(
        kinds.contains_key("I1"),
        "expected a single-ownership violation; got {kinds:?}",
    );
}

/// The fourth self-validation gate: the balance path allowed to hand on a task
/// whose kill bit is already set.
///
/// This is the shape of the panic the owner's T14 took at 949 s of uptime, with
/// doom exiting — `retire_task: task not released after 1s: InTransit(CpuId(1))`
/// — and the reason `CpuSched::hand_off` now reads the kill bit. `InTransit` is
/// the one state whose reap is carried by an adopt rather than by the retire:
/// the retire is `Urgency::Preempt` and always kicks, the adopt of a non-RT task
/// is `Urgency::Normal` and by design kicks nobody, and a destination that is
/// running owes the task nothing until its next voluntary pass. Handing a corpse
/// there buys the thief a dead task and pays for it with the retirer's deadline.
///
/// It must be caught by **I14** specifically. The failure rate is a rate and not
/// a certainty — the migration needs a boosted wake to land on the RT daemon's
/// CPU in the window between the teardown and that CPU's drain — so this asserts
/// a floor over the sweep rather than every seed.
#[test]
fn old_migrate_keeping_the_corpse_is_caught() {
    let scenario = scenarios::old_migrate_kept_the_corpse();
    let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
    let mut caught = 0;
    for seed in 0..SEEDS {
        let mut choices = if seed % 2 == 0 {
            ChoiceStream::from_seed(seed)
        } else {
            ChoiceStream::pct(seed, scenario.cpus, 3)
        };
        let outcome = run(scenario.clone(), &mut choices);
        if outcome.passed() {
            continue;
        }
        caught += 1;
        for violation in &outcome.violations {
            let id = violation.split(':').next().unwrap_or("?").to_string();
            *kinds.entry(id).or_default() += 1;
        }
    }
    assert!(
        caught > 0,
        "migrating a task that is already dead went undetected in {SEEDS} \
         schedules — I14 says nothing about the balance path it was written for",
    );
    assert!(
        kinds.contains_key("I14"),
        "expected a retire-promptness violation; got {kinds:?}",
    );
}

/// The positive half of the same pair: with the kill bit read, no schedule of
/// that workload puts a corpse in transit, and every retire completes well
/// inside the derived bound.
///
/// The measurement is the point. `retire_task`'s guard is a wall clock two
/// orders of magnitude wider than [`toyos_sched_sim::explore::Outcome::retire_bound`],
/// so what this reports is how much of that budget the protocol actually spends
/// — and a change that starts spending it shows up here as a number long before
/// it shows up on the owner's laptop as a panic.
#[test]
fn a_retire_completes_inside_its_derived_bound() {
    let scenario = scenarios::retire_under_balance();
    let mut worst = 0;
    let mut bound = 0;
    for seed in 0..SEEDS {
        let mut choices = if seed % 2 == 0 {
            ChoiceStream::from_seed(seed)
        } else {
            ChoiceStream::pct(seed, scenario.cpus, 3)
        };
        let outcome = run(scenario.clone(), &mut choices);
        assert!(outcome.passed(), "{}", outcome.report());
        if outcome.retire_latency > worst {
            worst = outcome.retire_latency;
            bound = outcome.retire_bound;
        }
    }
    assert!(
        worst > 0,
        "in {SEEDS} schedules no retire ever outlived the instant it was posted \
         in, so I14's latency half never measured anything",
    );
    println!("I14: worst retire {worst} ns against a {bound} ns bound");
}

/// A `Retire` that lands inside the registration window, and the fact that the
/// workload driver no longer has a check of its own for it.
///
/// `Vm::block_pass` used to cancel the ticket and exit when it found the kill
/// bit set. That was a compensation: the kernel's `pass_block` had no such
/// check, so the simulator was papering over a hole rather than modelling it —
/// the drain answers a `Retire` aimed at the *running* task with `need_resched`
/// and consumes it, and a task that then parks is never picked, never reaped,
/// and holds its address space forever. Deleting that arm reproduced it in 3+
/// of 400 `crash_md_exit_race` seeds.
///
/// The arm is gone and the sweeps are clean, which is only evidence about
/// `WaitTicket::commit` if the case still *happens*. So it is counted.
#[test]
fn a_retire_inside_the_registration_window_is_honoured_by_the_core() {
    let mut killed = 0;
    for seed in 0..SEEDS {
        let mut choices = if seed % 2 == 0 {
            ChoiceStream::from_seed(seed)
        } else {
            ChoiceStream::pct(seed, 2, 3)
        };
        let outcome = run(scenarios::crash_md_exit_race(), &mut choices);
        assert!(outcome.passed(), "{}", outcome.report());
        killed += outcome.killed_at_park;
    }
    assert!(
        killed > 0,
        "in {SEEDS} schedules no retire ever landed inside a registration \
         window, so `Commit::Killed` is dead code here and these runs say \
         nothing about the hole that arm used to hide",
    );
}

/// The third self-validation gate: the registration window with preemption
/// left enabled, which is what the kernel had until the wait ticket grew a
/// guard.
///
/// This one is the reason `Vm::enabled` withholds `Step::Pass` while a CPU is
/// mid-block. That withholding is a *model* of the kernel's preempt count, and
/// a model nobody can falsify is a comment: flip the scenario's window to
/// `Preemptible` and the same harness executes the step, on the same
/// schedules, and the core aborts. It aborts rather than reporting a violation
/// because a task whose word reads `Committing` has no legal preempt edge —
/// which is exactly why the window had to be closed rather than tolerated.
/// Teaching `RunningTask::preempt` to accept `Committing` would publish
/// `Ready`, and every waker that pops the registration would then report
/// `Claim::Lost` and move on: a lost wake, silently, in place of a panic.
#[test]
fn a_pass_inside_the_registration_window_is_caught() {
    let scenario = scenarios::old_preemptible_window();
    let caught = sweep::abort_gate(&scenario, SEEDS);
    let Some((seed, message)) = caught else {
        panic!(
            "an involuntary pass inside the registration window went undetected \
             in {SEEDS} schedules — the preempt guard the kernel holds there is \
             then unfalsifiable, and so is this model of it",
        );
    };
    assert!(
        message.contains("disagrees with its state word") && message.contains("Committing"),
        "expected the running-task word check to be what fires (seed {seed}); got: {message}",
    );

    // And the control: the identical workload with the guard modelled comes
    // back clean over the same seeds, so the gate is measuring the guard and
    // not the workload.
    let guarded = sweep::seed_sweep(&scenarios::crash_md_exit_race(), SEEDS, 1);
    assert!(guarded.passed(), "{}", guarded.report());
}

/// §8.1's *residual* window, which the fix names and deliberately does not
/// close: a waker may claim the task in the instructions between the commit
/// publishing `Blocked` and the park itself. That is why `RunningTask::park`
/// accepts `WakeQueued` — and until the block became two steps, no simulator
/// run had ever executed it, so that acceptance was a claim backed by nothing.
///
/// The window cannot be a step boundary (a `SchedPass` borrows `CpuSched` and
/// cannot be held across one), so it is reached by injection. What this test
/// asserts is that it is reached *at all*: an assertion that the arm is
/// exercised, not that it is correct — the sweeps say the latter.
#[test]
fn the_park_sees_claims_that_landed_after_its_commit() {
    let mut claims = 0;
    for seed in 0..SEEDS {
        let mut choices = ChoiceStream::from_seed(seed);
        let outcome = run(scenarios::lost_wake_pipe(), &mut choices);
        assert!(outcome.passed(), "{}", outcome.report());
        claims += outcome.pre_park_claims;
    }
    assert!(
        claims > 0,
        "in {SEEDS} schedules no park ever saw a claim land after its own \
         commit, so `RunningTask::park`'s `WakeQueued` arm is dead code here \
         and the residual window is back outside the model",
    );
}

/// The control that turns "the simulator structurally could not see this" from
/// a claim into a measurement.
///
/// `old_commit_fused` is the *same* workload and the *same* pre-`8508b37`
/// blocking shape, with one difference: the call site and the pass are one VM
/// step, which is what this simulator did until the block was split. Nothing
/// can interleave inside a step, so the window is not in the step relation and
/// every schedule comes back clean — which is exactly why the harness
/// certified a protocol whose lost wake it could not execute.
#[test]
fn blind_spot_needed_the_step_split() {
    let fused = sweep::seed_sweep(&scenarios::old_commit_fused(), SEEDS, 1);
    assert!(
        fused.passed(),
        "with the two halves fused into one step the bug is invisible; if this \
         now fails, the control has stopped controlling for anything: {}",
        fused.report(),
    );

    let split = sweep::seed_sweep(&scenarios::old_commit_before_pass(), SEEDS, 1);
    assert!(
        !split.passed(),
        "the identical shape with the halves as separate steps must fail: {}",
        split.report(),
    );
}

/// The A/B that makes the gate a comparison rather than an assertion: the
/// *same* schedule, run against both protocols. Only the algorithm differs.
#[test]
fn the_new_protocol_survives_the_schedule_that_breaks_the_old_one() {
    let decisions: Vec<usize> =
        shrink::decode(include_str!("../corpus/old_steal_port_i8.trace")).decisions;

    let mut old = ChoiceStream::replay(decisions.clone());
    let old = run(scenarios::old_steal_port(), &mut old);
    assert!(
        !old.passed(),
        "the old protocol must fail this schedule; it passed",
    );
    assert!(
        old.violations.iter().any(|v| v.starts_with("I8")),
        "expected the address space to be freed under a live task: {:?}",
        old.violations,
    );

    let mut new = ChoiceStream::replay(decisions);
    let new = run(scenarios::crash_md_exit_race(), &mut new);
    assert!(
        new.passed(),
        "the new protocol must survive the very schedule that breaks the old \
         one: {}",
        new.report(),
    );
}

/// Committed traces are permanent regressions, including the negative one.
#[test]
fn corpus_traces_still_do_what_they_were_committed_for() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/corpus");
    let mut checked = 0;
    for entry in std::fs::read_dir(dir).expect("the corpus directory exists") {
        let path = entry.expect("readable entry").path();
        if path.extension().is_none_or(|e| e != "trace") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("readable trace");
        let entry = shrink::decode(&text);
        let scenario =
            scenarios::by_name(&entry.scenario).expect("the trace names a known scenario");
        let outcome = shrink::replay(&entry, scenario);
        assert_eq!(
            !outcome.passed(),
            entry.expect_failure,
            "{}: expected {}, got {}",
            path.display(),
            if entry.expect_failure {
                "failure"
            } else {
                "success"
            },
            outcome.report(),
        );
        checked += 1;
    }
    assert!(
        checked >= 3,
        "expected the committed corpus, found {checked}"
    );
}

/// Determinism is the property everything else rests on: replaying a run's
/// decisions must reproduce it exactly, whatever driver produced it.
#[test]
fn a_run_replays_exactly() {
    for scenario in scenarios::all() {
        let mut original = ChoiceStream::pct(42, scenario.cpus, 3);
        let first = run(scenario.clone(), &mut original);
        let mut replay = ChoiceStream::replay(first.decisions.clone());
        let again = run(scenario.clone(), &mut replay);
        assert_eq!(
            (first.steps, first.elapsed, first.switches, first.kicks),
            (again.steps, again.elapsed, again.switches, again.kicks),
            "{} did not replay identically",
            scenario.name,
        );
        assert_eq!(first.decisions, again.decisions, "{}", scenario.name);
    }
}

/// A shrunk trace is only useful if it still fails, and only readable if it
/// is much shorter than what it came from.
#[test]
fn shrinking_keeps_the_failure_and_loses_the_noise() {
    let scenario = scenarios::old_steal_port();
    // A failure the explorer trips over in two steps has no noise to lose;
    // what the shrinker is for is only visible on one it took a while to
    // reach.
    let outcome = (0..SEEDS)
        .find_map(|seed| {
            let mut choices = ChoiceStream::from_seed(seed);
            let outcome = run(scenario.clone(), &mut choices);
            (!outcome.passed() && outcome.steps > 16).then_some(outcome)
        })
        .expect("some seed must reach the old protocol's failure the long way");
    let minimized = shrink::shrink(&scenario, outcome.decisions.clone());
    assert!(
        minimized.len() < outcome.decisions.len(),
        "shrinking removed nothing: {} decisions",
        minimized.len(),
    );

    let mut replay = ChoiceStream::replay(minimized);
    assert!(
        !run(scenario, &mut replay).passed(),
        "the shrunk trace must still fail",
    );
}

/// `cpus = 1` is first-class (spec §11 Stage 4): it is the configuration Doom
/// runs in, and the one where a scheduling mistake is audible.
#[test]
fn the_audio_pipeline_holds_on_one_cpu() {
    let result = sweep::seed_sweep(&scenarios::audio_pipeline(1), SEEDS, 3);
    assert!(result.passed(), "{}", result.report());
    assert_eq!(result.scenario, "audio_pipeline");
}

/// Invariant I5 is *alive*, which is a separate claim from I5 being green.
///
/// A fairness check whose contention windows never open reports "clean" on every
/// scenario in the tree and certifies nothing — which is exactly the shape of
/// gate A's four instrument defects, and exactly what would happen here if the
/// window conditions (same runnable set, saturated machine, empty RT band) were
/// one degree stricter than the workload can satisfy. So the spread is required
/// to be *non-zero*: some window has to have opened, stayed open long enough for
/// a real separation to accumulate, and been measured.
///
/// The widths are the two `all()` carries. Spec §11 Stage 9 wants 1–128, which
/// runs from the CLI as `measure fairness_storm:<cpus>`; the sweep here is what
/// `cargo test` can afford.
///
/// Both widths meet the *derived* bound today — 30/60 ms and 102/108 ms at
/// 10 000 seeds — so `worst_over_bound` must be zero here, and that is asserted:
/// if the recorded allowance ever starts carrying these two, the suite says so
/// rather than passing quietly on it.
///
/// **And the *reach* is asserted, not only the verdict** — I13's gate, which I5
/// went without until this test grew one. A non-zero spread says a window opened
/// *somewhere*; it does not say I5 was watching for any meaningful part of the
/// run, and one window a nanosecond wide satisfies it. That gap matters more for
/// I5 than for I13, because I5's window has four ways to close — an RT task
/// present, any CPU idle, the member set changing, a member under its even share
/// — against I13's one, and every one of them is a property the pick or the
/// placement can narrow without a single verdict going red. The recorded figures
/// are 99% and 81% of executed time (`measure fairness_storm:<cpus> 100`, on
/// `a0729cf` with this change applied); a halving reds.
#[test]
fn the_fairness_storm_is_measured_and_holds() {
    for (cpus, reach) in [(1, 99), (2, 81)] {
        let result = sweep::seed_sweep(&scenarios::fairness_storm(cpus), FAIR_SEEDS, 3);
        assert!(
            result.fair_coverage_pct() * 2 >= reach,
            "at {cpus} cpu(s) invariant I5 had a comparison open for {}% of the \
             run against {reach}% recorded. Its *reach* has collapsed, which is \
             not the same as its verdict being clean — read this as loudly as a \
             violation and find what closed the windows: {}",
            result.fair_coverage_pct(),
            result.report(),
        );
        assert!(result.passed(), "{}", result.report());
        assert!(
            result.worst_fair_spread > 0,
            "at {cpus} cpu(s) invariant I5 never opened a contention window it \
             could measure, so its clean verdict on every other scenario is a \
             verdict about nothing: {}",
            result.report(),
        );
        assert_eq!(
            result.worst_over_bound, 0,
            "at {cpus} cpu(s) the shipped scheduler has started crossing the \
             *derived* fairness bound and is passing on the recorded allowance \
             alone. That is a regression against the standard even though the \
             gate is green — see scenarios::FAIRNESS_SAMPLE: {}",
            result.report(),
        );
        // And the measurement has to be a *comparison*, not an accident of a
        // bound so wide nothing could reach it. Both widths sit within a factor
        // of two of the derived bound today, so a factor of four here is slack
        // and not a target.
        assert!(
            result.worst_fair_spread * 4 > result.worst_fair_bound,
            "at {cpus} cpu(s) the worst spread is more than 4x under its own \
             bound; the bound has stopped constraining anything: {}",
            result.report(),
        );
    }
}

/// The fifth self-validation gate: one fair share per *thread* — spec §13.9's
/// rejected policy — instead of one per process.
///
/// `trio` has three times `solo`'s threads and exactly the same entitlement.
/// Under the shipped policy they split the CPU evenly; under per-thread shares
/// `trio` takes three quarters. If I5 cannot see a 3:1 split of a two-way share,
/// it is not measuring fairness, and every clean I5 report in this file means
/// nothing.
#[test]
fn per_thread_fair_shares_are_caught() {
    let broken = scenarios::fair_share_per_thread();
    let mut caught = 0;
    let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
    for seed in 0..FAIR_SEEDS {
        let mut choices = ChoiceStream::from_seed(seed);
        let outcome = run(broken.clone(), &mut choices);
        if outcome.passed() {
            continue;
        }
        caught += 1;
        for violation in &outcome.violations {
            let id = violation.split(':').next().unwrap_or("?").to_string();
            *kinds.entry(id).or_default() += 1;
        }
    }
    assert_eq!(
        caught, FAIR_SEEDS as usize,
        "a process taking three quarters of the machine off its thread count \
         went undetected in {}/{FAIR_SEEDS} schedules",
        FAIR_SEEDS as usize - caught,
    );
    assert!(
        kinds.contains_key("I5"),
        "expected the service-spread violation; got {kinds:?}",
    );
    assert_eq!(
        kinds.len(),
        1,
        "per-thread shares must be caught by I5 and not as collateral in some \
         other invariant — I6 in particular sums over a process's shares and \
         must stay correct under either shape: {kinds:?}",
    );

    // The control: the identical workload under the shipped policy, or the gate
    // is detecting the workload rather than the policy.
    let shipped = sweep::seed_sweep(&scenarios::fairness_storm(1), FAIR_SEEDS, 1);
    assert!(shipped.passed(), "{}", shipped.report());
}

/// The sixth self-validation gate, and I5's other direction: a share charged
/// twice for every nanosecond it runs.
///
/// It matters that this is a *separate* gate from `per_thread_fair_shares`, and
/// that the two fail in opposite directions. Fairness is service measured
/// against entitlement, and there are two ways to get it wrong — mis-attribute
/// the entitlement, or mis-measure the service. A check that saw only one of
/// them would be half an instrument, and the half it was missing would be
/// invisible.
#[test]
fn double_charging_a_share_is_caught() {
    let broken = scenarios::fair_double_charge();
    let mut caught = 0;
    let mut throttled = 0;
    for seed in 0..FAIR_SEEDS {
        let mut choices = ChoiceStream::from_seed(seed);
        let outcome = run(broken.clone(), &mut choices);
        if outcome.passed() {
            continue;
        }
        caught += 1;
        // The direction is the point: the double-charged process gets *less*,
        // which is the opposite of what per-thread shares do to it.
        if outcome
            .violations
            .iter()
            .any(|v| v.starts_with("I5") && v.contains("solo=") && v.contains("trio="))
        {
            let text = outcome.violations.join(" ");
            let value = |name: &str| -> u64 {
                text.split(name)
                    .nth(1)
                    .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0)
            };
            if value("solo=") > value("trio=") {
                throttled += 1;
            }
        }
    }
    assert_eq!(
        caught, FAIR_SEEDS as usize,
        "a share throttled for work it never did went undetected in {}/{FAIR_SEEDS} \
         schedules",
        FAIR_SEEDS as usize - caught,
    );
    assert_eq!(
        throttled, caught,
        "the double-charged process must come out *behind*; if it is ahead, this \
         gate is catching something other than the charge",
    );

    let shipped = sweep::seed_sweep(&scenarios::fairness_storm(1), FAIR_SEEDS, 1);
    assert!(shipped.passed(), "{}", shipped.report());
}

/// The seventh self-validation gate: the core's `feature = "check"` pass-duration
/// assert, which spec §10.2 makes the on-target counterpart to everything else in
/// this file.
///
/// It is the one check here that says something about *cost* rather than about
/// state, so it is the one the simulator cannot exercise for free: the VM's clock
/// does not move inside a step, and an assert whose measured quantity is always
/// zero is an assert that cannot fail. `SimHw` therefore charges every pass a
/// modelled cost, and this gate turns that cost up past the budget.
///
/// Without it, "kernel check builds assert a max pass duration" would be a claim
/// backed by an expression that computes `0 <= 200_000` a few thousand times a
/// second.
#[test]
fn a_pass_that_overruns_its_budget_is_caught() {
    let caught = sweep::abort_gate(&scenarios::overlong_pass(), FAIR_SEEDS);
    let Some((seed, message)) = caught else {
        panic!(
            "a pass modelled at five times `cpu::MAX_PASS_NS` went undetected in \
             {FAIR_SEEDS} schedules — either the assert is not compiled in, or the \
             clock it reads never moves",
        );
    };
    assert!(
        message.contains("invariant P: a scheduler pass took"),
        "expected the pass-duration assert to be what fires (seed {seed}); got: {message}",
    );

    // The control: the identical workload at the default modelled cost of zero
    // must be clean, so the gate is measuring the budget and not the workload.
    let free = sweep::seed_sweep(&scenarios::lost_wake_pipe(), FAIR_SEEDS, 1);
    assert!(free.passed(), "{}", free.report());
}

/// The fourth self-validation gate, and the newest: invariant I9's teeth.
///
/// I9 says one lend buys at most one quantum of running time at the borrowed
/// priority. Commit `9c2fc4d` shipped a park that cleared the window only
/// `if now >= until`, so a lend blocked on before it ran out survived the
/// block — and with `RtState::arm` re-arming at every dispatch, a task that
/// obtains one lend and thereafter runs less than a quantum before blocking
/// holds inherited RT forever, off a single pipe interaction and with nobody
/// renewing anything.
///
/// The invariant I9 that shipped alongside it could not see this, and the
/// giveaway was that it needed no change: it compared a *running* task's
/// `until` against the clock, and a re-armed `until` is by construction fresh.
/// A check that passes because it stopped measuring is the same failure mode as
/// gate A's four instrument defects, so I9 is now the cumulative form and this
/// test is what says so. If it ever stops failing, the check has lost its teeth
/// again and every clean I9 report above it means nothing.
#[test]
fn old_park_keeping_the_lend_is_caught() {
    let scenario = scenarios::old_park_kept_the_lend().with_park(ParkShape::KeepLapsedLend);
    let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
    let mut caught = 0;
    for seed in 0..SEEDS {
        let mut choices = ChoiceStream::from_seed(seed);
        let outcome = run(scenario.clone(), &mut choices);
        if outcome.passed() {
            continue;
        }
        caught += 1;
        for violation in &outcome.violations {
            let id = violation.split(':').next().unwrap_or("?").to_string();
            *kinds.entry(id).or_default() += 1;
        }
    }
    assert!(
        caught > 0,
        "a task holding a borrowed RT priority forever off one lend went \
         undetected in {SEEDS} schedules — invariant I9 has no teeth, and \
         nothing it certifies elsewhere means anything",
    );
    assert!(
        kinds.contains_key("I9"),
        "expected the per-lend running-time violation; got {kinds:?}",
    );

    // The control: the same workload under the shipped park must be clean, or
    // the gate is only detecting the workload.
    let fixed = scenarios::old_park_kept_the_lend();
    for seed in 0..SEEDS {
        let mut choices = ChoiceStream::from_seed(seed);
        let outcome = run(fixed.clone(), &mut choices);
        assert!(
            outcome.passed(),
            "the shipped park failed its own gate's workload on seed {seed}: {:?}",
            outcome.violations,
        );
    }
}

/// Invariant I13 is *alive*, which is a separate claim from I13 being green —
/// and, unlike I5, it is a claim nothing else in this file would notice was
/// false. I5 measures service per *process*, so it reports a perfectly even
/// split while one thread of a share never runs; if I13's windows never opened,
/// every fairness verdict in this tree would still read clean and sibling
/// starvation would be unguarded.
///
/// So the spread is required to be non-zero on all three widths of the fairness
/// workload — some window has to have opened, stayed open, and been measured —
/// and to be within a factor of four of its own bound somewhere, or the bound
/// has stopped constraining anything.
///
/// **And the *reach* is asserted, not only the verdict.** I13's window closes
/// when a member's threads stop being spread evenly over the CPUs, so a change
/// to the pick or the balance can make this check measure less instead of
/// failing — a live gate switched off by the thing it guards. The recorded
/// figures are 96%, 69% and 99% of executed time (`measure <scenario> 100`, on
/// `be4b34a` with this change applied); a halving reds. The falloff with width
/// is real and worth knowing: 55% at four CPUs and 45% at eight, because
/// threads exit at slightly different moments and unbalance the machine sooner
/// the wider it is. All three meet the *derived* bound today
/// (the recorded sample on `scenarios::fair_workload`), so
/// `worst_thread_over_bound` must be zero, and that is asserted: if the recorded allowance ever starts carrying these, the
/// suite says so rather than passing quietly on it.
#[test]
fn invariant_i13_is_measured_and_holds() {
    let mut tightest = u64::MAX;
    for (scenario, reach) in [
        (scenarios::fairness_storm(1), 96),
        (scenarios::fairness_storm(2), 69),
        (scenarios::sibling_storm(), 99),
    ] {
        let name = scenario.name;
        let result = sweep::seed_sweep(&scenario, FAIR_SEEDS, 3);
        assert!(
            result.thread_coverage_pct() * 2 >= reach,
            "{name}: invariant I13 had a comparison open for {}% of the run \
             against {reach}% recorded. Its *reach* has collapsed, which is not \
             the same as its verdict being clean — read this as loudly as a \
             violation and find what closed the windows: {}",
            result.thread_coverage_pct(),
            result.report(),
        );
        assert!(result.passed(), "{}", result.report());
        assert!(
            result.worst_thread_spread > 0,
            "{name}: invariant I13 never opened a contention window it could \
             measure, so its clean verdict on every other scenario is a verdict \
             about nothing: {}",
            result.report(),
        );
        assert_eq!(
            result.worst_thread_over_bound, 0,
            "{name}: the shipped scheduler has started crossing the *derived* \
             per-thread bound and is passing on the recorded allowance alone. \
             That is a regression against the standard even though the gate is \
             green — see the recorded sample on scenarios::fair_workload: {}",
            result.report(),
        );
        tightest = tightest.min(result.worst_thread_bound / result.worst_thread_spread.max(1));
    }
    assert!(
        tightest <= 4,
        "every fairness width sat more than 4x under the per-thread bound \
         (closest was {tightest}x); the bound has stopped constraining anything",
    );
}

/// The eighth self-validation gate, and the one I5 is structurally incapable of
/// standing in for: within a share, the *lowest-keyed* ready thread is
/// dispatched every time instead of the earliest-inserted one.
///
/// It **must fail**, and it must fail on I13 and nothing else. That second half
/// is the whole point. A share's pot is charged for the time the *process* ran,
/// not for which of its threads ran it, so this ordering leaves the per-process
/// split exactly as it was: `solo` and `trio` still take half the machine each,
/// I5 still reports an even split, I6's refcounts are still right — and two of
/// `trio`'s three threads never run at all. If I5 ever starts firing here as
/// well, this has stopped being a test of the thing I13 was built for.
#[test]
fn identity_ordering_within_a_share_is_caught() {
    let broken = scenarios::fair_identity_within_share();
    let mut caught = 0;
    let mut starved = 0;
    let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
    for seed in 0..FAIR_SEEDS {
        let mut choices = ChoiceStream::from_seed(seed);
        let outcome = run(broken.clone(), &mut choices);
        if outcome.passed() {
            continue;
        }
        caught += 1;
        // A starved sibling, not merely an uneven one: the report has to show a
        // thread of the share on zero nanoseconds of service.
        if outcome.violations.iter().any(|v| v.contains("=0")) {
            starved += 1;
        }
        for violation in &outcome.violations {
            let id = violation.split(':').next().unwrap_or("?").to_string();
            *kinds.entry(id).or_default() += 1;
        }
    }
    assert_eq!(
        caught, FAIR_SEEDS as usize,
        "a share serving one of its three threads and starving the other two \
         went undetected in {}/{FAIR_SEEDS} schedules",
        FAIR_SEEDS as usize - caught,
    );
    assert_eq!(
        starved, caught,
        "the gate fired without a sibling on zero service; it is catching \
         something other than starvation",
    );
    assert_eq!(
        kinds.len(),
        1,
        "sibling starvation must be caught by I13 and by I13 alone — I5 sees an \
         even per-process split here, which is exactly why I13 exists: {kinds:?}",
    );
    assert!(
        kinds.contains_key("I13"),
        "expected the per-thread service violation; got {kinds:?}",
    );

    // The control: the identical workload under the shipped ordering, or the
    // gate is detecting the workload rather than the ordering.
    let shipped = sweep::seed_sweep(&scenarios::sibling_storm(), FAIR_SEEDS, 1);
    assert!(shipped.passed(), "{}", shipped.report());
}

/// The control that makes `identity_ordering_within_a_share_is_caught` a
/// measurement rather than a guess about which break was needed — and a finding
/// in its own right.
///
/// `queue.rs` says the fair band's tie-break must not be `TaskKey` because "the
/// same thread wins every tie and the others only run when it blocks". Ported
/// exactly as written, that ordering is **invisible**: a share's pot is charged
/// for every nanosecond any of its threads runs, so a thread re-inserted after
/// a dispatch already carries a key strictly above every sibling queued before
/// it. The ordering is insertion order whatever the tie-break is; exact ties
/// survive only where no charge separates two inserts, and one dispatch
/// dissolves them.
///
/// So this asserts two things: that the tie-break really is a different
/// scheduler — some seed must produce a different run, or the mode is a no-op
/// and proves nothing — and that I13 nonetheless comes back clean on every one.
#[test]
fn an_identity_tiebreak_changes_the_schedule_and_not_the_sibling_split() {
    let shipped = scenarios::sibling_storm();
    let broken = scenarios::fair_identity_tiebreak();
    let mut diverged = 0;
    for seed in 0..FAIR_SEEDS {
        let mut choices = ChoiceStream::from_seed(seed);
        let with = run(broken.clone(), &mut choices);
        assert!(
            with.passed(),
            "the identity tie-break is expected to be invisible to I13; if it \
             now fails, the control has stopped controlling and the second gate \
             may no longer be the one that was needed: {}",
            with.report(),
        );
        let mut choices = ChoiceStream::from_seed(seed);
        let without = run(shipped.clone(), &mut choices);
        assert!(without.passed(), "{}", without.report());
        if (with.steps, with.switches, with.fair_spread)
            != (without.steps, without.switches, without.fair_spread)
        {
            diverged += 1;
        }
    }
    assert!(
        diverged > 0,
        "the identity tie-break produced an identical run on all {FAIR_SEEDS} \
         seeds, so `FairOrder::IdentityTiebreak` is not reaching the queue at \
         all and its clean verdict says nothing",
    );
}
