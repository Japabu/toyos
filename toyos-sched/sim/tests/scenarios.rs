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
/// exploration with all of I1–I12 checked after every step.
const SEEDS: u64 = 500;

/// Steps per scenario in the in-test fuzz sweep.
const FUZZ_STEPS: u64 = 20_000;

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
