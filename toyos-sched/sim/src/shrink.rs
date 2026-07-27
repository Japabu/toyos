//! Delta-debugging minimizer and replay emitter — spec §10.3.
//!
//! Determinism is what makes this cheap: a candidate decision list is not an
//! approximation of a schedule, it *is* the schedule, so every candidate can
//! be evaluated exactly. Shrinking a nondeterministic repro means re-running
//! it until it fails again; shrinking this one means running it once.

use crate::choice::ChoiceStream;
use crate::explore::{run, Outcome};
use crate::workload::Scenario;

/// Does this decision list still reproduce a failure?
fn fails(scenario: &Scenario, decisions: &[usize]) -> bool {
    let mut choices = ChoiceStream::replay(decisions.to_vec());
    !run(scenario.clone(), &mut choices).passed()
}

/// Shrink a failing decision list to a local minimum: repeatedly try deleting
/// contiguous chunks, halving the chunk size when a full pass finds nothing.
///
/// Truncation is tried first and matters most — a failure that is reached
/// after N decisions rarely needs the tail, and dropping it turns a
/// thousand-step trace into a readable one.
pub fn shrink(scenario: &Scenario, decisions: Vec<usize>) -> Vec<usize> {
    assert!(
        fails(scenario, &decisions),
        "shrink: the input trace does not fail",
    );
    let mut best = decisions;

    // Binary-search the shortest failing prefix.
    let (mut low, mut high) = (0usize, best.len());
    while low < high {
        let mid = (low + high) / 2;
        if fails(scenario, &best[..mid]) {
            high = mid;
        } else {
            low = mid + 1;
        }
    }
    if low < best.len() && fails(scenario, &best[..low]) {
        best.truncate(low);
    }

    let mut chunk = (best.len() / 2).max(1);
    while chunk >= 1 {
        let mut index = 0;
        while index < best.len() {
            let end = (index + chunk).min(best.len());
            let mut candidate = best.clone();
            candidate.drain(index..end);
            if fails(scenario, &candidate) {
                best = candidate;
            } else {
                index += chunk;
            }
        }
        if chunk == 1 {
            break;
        }
        chunk /= 2;
    }
    best
}

/// The on-disk corpus format: a header naming the scenario and the expected
/// outcome, then the decisions. Committed traces are permanent regressions —
/// including the negative one, whose expectation is `fail`.
pub fn encode(scenario: &str, expect_failure: bool, decisions: &[usize]) -> String {
    let mut out = format!(
        "# scenario: {scenario}\n# expect: {}\n",
        if expect_failure { "fail" } else { "pass" },
    );
    for (index, decision) in decisions.iter().enumerate() {
        out.push_str(&decision.to_string());
        out.push(if (index + 1) % 32 == 0 { '\n' } else { ' ' });
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

pub struct CorpusEntry {
    pub scenario: String,
    pub expect_failure: bool,
    pub decisions: Vec<usize>,
}

pub fn decode(text: &str) -> CorpusEntry {
    let mut scenario = String::new();
    let mut expect_failure = false;
    let mut decisions = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("# scenario:") {
            scenario = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("# expect:") {
            expect_failure = rest.trim() == "fail";
        } else if !line.starts_with('#') {
            decisions.extend(line.split_whitespace().map(|token| {
                token
                    .parse::<usize>()
                    .expect("corpus decisions are integers")
            }));
        }
    }
    assert!(!scenario.is_empty(), "corpus entry without a scenario");
    CorpusEntry {
        scenario,
        expect_failure,
        decisions,
    }
}

/// Replay a corpus entry and check it still does what it was committed for.
pub fn replay(entry: &CorpusEntry, scenario: Scenario) -> Outcome {
    let mut choices = ChoiceStream::replay(entry.decisions.clone());
    run(scenario, &mut choices)
}
