//! Seed and fuzz-byte sweeps — the exit criterion of spec §11's Stage 4.
//!
//! The criterion is "10⁴ seeds + 10⁷ fuzz steps per scenario class with zero
//! invariant violations". Both budgets are parameters here so that the same
//! code runs the small sweep `cargo test` can afford and the full one the
//! gate asks for; what must never differ between them is the checking.

use crate::choice::ChoiceStream;
use crate::explore::{run, run_catching, Outcome};
use crate::workload::Scenario;

pub struct SweepResult {
    pub scenario: &'static str,
    pub runs: usize,
    pub steps: u64,
    pub failures: Vec<Outcome>,
    /// Worst invariant-I5 service spread across the sweep, and the bound in
    /// force when it happened. The sweep's fairness *measurement*, which is what
    /// spec §11 Stage 9 compares between frontier implementations; `passed()`
    /// only says it stayed under the bound.
    pub worst_fair_spread: u64,
    pub worst_fair_bound: u64,
    /// Worst spread past the derived bound anywhere in the sweep. Non-zero is
    /// the standard-versus-shipped gap, not a test failure — the run passed on
    /// the recorded allowance.
    pub worst_over_bound: u64,
}

impl SweepResult {
    pub fn passed(&self) -> bool {
        self.failures.is_empty()
    }

    /// Fold one run's fairness measurement in. Kept next to `passed()` so a new
    /// sweep entry point cannot forget it.
    fn observe(&mut self, outcome: &Outcome) {
        if outcome.fair_spread > self.worst_fair_spread {
            self.worst_fair_spread = outcome.fair_spread;
            self.worst_fair_bound = outcome.fair_bound;
        }
        self.worst_over_bound = self.worst_over_bound.max(outcome.fair_over_bound);
    }

    pub fn report(&self) -> String {
        if self.passed() {
            format!(
                "{}: {} runs, {} steps, clean (I5 worst spread {}/{} ns{})",
                self.scenario,
                self.runs,
                self.steps,
                self.worst_fair_spread,
                self.worst_fair_bound,
                if self.worst_over_bound == 0 {
                    String::new()
                } else {
                    format!(
                        ", {} ns PAST THE DERIVED BOUND on the recorded allowance",
                        self.worst_over_bound,
                    )
                },
            )
        } else {
            format!(
                "{}: {} runs, {} steps, {} FAILED\n{}",
                self.scenario,
                self.runs,
                self.steps,
                self.failures.len(),
                self.failures
                    .iter()
                    .take(3)
                    .map(|f| f.report())
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        }
    }
}

/// `seeds` seeded runs, alternating the uniform driver with PCT so both
/// exploration strategies contribute to the same budget.
pub fn seed_sweep(scenario: &Scenario, seeds: u64, keep_failures: usize) -> SweepResult {
    let mut result = SweepResult {
        scenario: scenario.name,
        runs: 0,
        steps: 0,
        failures: Vec::new(),
        worst_fair_spread: 0,
        worst_fair_bound: 0,
        worst_over_bound: 0,
    };
    for seed in 0..seeds {
        let mut choices = if seed % 2 == 0 {
            ChoiceStream::from_seed(seed)
        } else {
            ChoiceStream::pct(seed, scenario.cpus, 3)
        };
        let outcome = run(scenario.clone(), &mut choices);
        result.runs += 1;
        result.steps += outcome.steps as u64;
        result.observe(&outcome);
        if !outcome.passed() && result.failures.len() < keep_failures {
            result.failures.push(outcome);
        }
    }
    result
}

/// The negative gate whose failure is an *abort* rather than a verdict: run
/// seeded schedules until the core's own assertion fires, and report the first
/// one that does.
///
/// Only `old_preemptible_window` needs it. Everything the invariant walks find
/// is a recorded violation; a pass that lands inside the registration window
/// instead panics inside `check_cpu`, which is the correct failure and cannot
/// be counted the ordinary way.
pub fn abort_gate(scenario: &Scenario, seeds: u64) -> Option<(u64, String)> {
    for seed in 0..seeds {
        let mut choices = if seed % 2 == 0 {
            ChoiceStream::from_seed(seed)
        } else {
            ChoiceStream::pct(seed, scenario.cpus, 3)
        };
        if let Err(message) = run_catching(scenario.clone(), &mut choices) {
            return Some((seed, message));
        }
    }
    None
}

/// Raw-byte-driven runs until `budget` *steps* have been executed — the fuzz
/// half of the criterion. The bytes come from a seeded generator here; the
/// same entry point takes libFuzzer's bytes unchanged, which is the point of
/// the `Bytes` driver.
pub fn fuzz_sweep(scenario: &Scenario, budget: u64, keep_failures: usize) -> SweepResult {
    let mut result = SweepResult {
        scenario: scenario.name,
        runs: 0,
        steps: 0,
        failures: Vec::new(),
        worst_fair_spread: 0,
        worst_fair_bound: 0,
        worst_over_bound: 0,
    };
    let mut generator = 0x9E3779B97F4A7C15u64;
    while result.steps < budget {
        generator = generator
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let bytes = fuzz_bytes(generator, 4096);
        let mut choices = ChoiceStream::from_bytes(bytes);
        let outcome = run(scenario.clone(), &mut choices);
        result.runs += 1;
        result.steps += outcome.steps as u64;
        result.observe(&outcome);
        if !outcome.passed() && result.failures.len() < keep_failures {
            result.failures.push(outcome);
        }
    }
    result
}

fn fuzz_bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}
