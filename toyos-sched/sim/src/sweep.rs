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
    /// I5's *reach* over the whole sweep: virtual nanoseconds it had a
    /// comparison open for, against the nanoseconds the sweep executed. Its
    /// window has four ways to close — an RT task present, a CPU idle, the
    /// member set changing, a member under its even share — and any of them can
    /// be narrowed by a change to the pick or the placement without a single
    /// verdict going red, so this is the number to A/B across one.
    pub fair_covered_ns: u64,
    /// Invariant I13's three, in the same three roles: the worst service spread
    /// between threads of one share, the bound in force when it happened, and
    /// any crossing of the derived per-thread bound the allowance let pass.
    pub worst_thread_spread: u64,
    pub worst_thread_bound: u64,
    pub worst_thread_over_bound: u64,
    /// I13's *reach* over the whole sweep: virtual nanoseconds it had a
    /// comparison open for, against the nanoseconds the sweep executed. A
    /// change to the pick or the balance can close I13's windows instead of
    /// failing them, so this is the number to A/B across one — a collapse here
    /// means the gate went quiet, which is not the same as the gate passing.
    pub thread_covered_ns: u64,
    pub elapsed_ns: u64,
}

impl SweepResult {
    /// An empty sweep over `scenario`.
    ///
    /// One constructor for the same reason [`Self::observe`] is one fold: both
    /// entry points below build this, and a hand-written initialiser in each is
    /// a field a new measurement can be added to one of and forgotten in the
    /// other — where the symptom is a reach or a worst-case silently reading
    /// zero for half the criterion.
    fn new(scenario: &Scenario) -> Self {
        Self {
            scenario: scenario.name,
            runs: 0,
            steps: 0,
            failures: Vec::new(),
            worst_fair_spread: 0,
            worst_fair_bound: 0,
            worst_over_bound: 0,
            fair_covered_ns: 0,
            worst_thread_spread: 0,
            worst_thread_bound: 0,
            worst_thread_over_bound: 0,
            thread_covered_ns: 0,
            elapsed_ns: 0,
        }
    }

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
        self.fair_covered_ns += outcome.fair_covered_ns;
        if outcome.thread_spread > self.worst_thread_spread {
            self.worst_thread_spread = outcome.thread_spread;
            self.worst_thread_bound = outcome.thread_bound;
        }
        self.worst_thread_over_bound = self.worst_thread_over_bound.max(outcome.thread_over_bound);
        self.thread_covered_ns += outcome.thread_covered_ns;
        self.elapsed_ns += outcome.elapsed;
    }

    /// What fraction of the sweep's executed time I5 had a comparison open for,
    /// in percent. The reach, as one number to compare across a change.
    pub fn fair_coverage_pct(&self) -> u64 {
        if self.elapsed_ns == 0 {
            return 0;
        }
        self.fair_covered_ns * 100 / self.elapsed_ns
    }

    /// What fraction of the sweep's executed time I13 had a comparison open
    /// for, in percent. The reach, as one number to compare across a change.
    pub fn thread_coverage_pct(&self) -> u64 {
        if self.elapsed_ns == 0 {
            return 0;
        }
        self.thread_covered_ns * 100 / self.elapsed_ns
    }

    pub fn report(&self) -> String {
        if self.passed() {
            format!(
                "{}: {} runs, {} steps, clean (I5 worst spread {}/{} ns{}, I5 reach {}%, \
                 I13 worst spread {}/{} ns{}, I13 reach {}%)",
                self.scenario,
                self.runs,
                self.steps,
                self.worst_fair_spread,
                self.worst_fair_bound,
                past_the_bound(self.worst_over_bound),
                self.fair_coverage_pct(),
                self.worst_thread_spread,
                self.worst_thread_bound,
                past_the_bound(self.worst_thread_over_bound),
                self.thread_coverage_pct(),
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

/// The standard-versus-shipped gap, in the one place both fairness invariants
/// report it, so neither can render it differently from the other.
fn past_the_bound(over: u64) -> String {
    if over == 0 {
        return String::new();
    }
    format!(", {over} ns PAST THE DERIVED BOUND on the recorded allowance")
}

/// `seeds` seeded runs, alternating the uniform driver with PCT so both
/// exploration strategies contribute to the same budget.
pub fn seed_sweep(scenario: &Scenario, seeds: u64, keep_failures: usize) -> SweepResult {
    let mut result = SweepResult::new(scenario);
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
    let mut result = SweepResult::new(scenario);
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
