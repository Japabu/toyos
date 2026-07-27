//! The step chooser and the trace recorder — spec §10.3.
//!
//! One loop: compute the enabled steps, ask the [`ChoiceStream`] which one to
//! take, take it, re-check every invariant. A run is completely determined by
//! its decisions, so a failure is a *value* — a list of integers — that can be
//! replayed, shrunk and committed as a regression.

use crate::choice::ChoiceStream;
use crate::invariants;
use crate::vm::{build_queues, Step, Vm};
use crate::workload::Scenario;

/// What a step belongs to, for the PCT driver's priorities: the vcpu whose
/// progress it represents. Clock jumps and device interrupts belong to no
/// vcpu.
pub fn actor(step: &Step) -> Option<usize> {
    match step {
        Step::Exec(cpu)
        | Step::Pass(cpu)
        | Step::DeliverIpi(cpu)
        | Step::FireTimer(cpu)
        | Step::OldInstall(cpu) => Some(*cpu),
        Step::OldSteal { thief, .. } => Some(*thief),
        Step::DeviceIrq(_) | Step::Advance => None,
    }
}

pub struct Outcome {
    pub scenario: &'static str,
    pub steps: usize,
    pub violations: Vec<String>,
    /// The decision sequence that produced this run — the replay input.
    pub decisions: Vec<usize>,
    /// Virtual nanoseconds elapsed.
    pub elapsed: u64,
    pub switches: u64,
    pub kicks: u64,
}

impl Outcome {
    pub fn passed(&self) -> bool {
        self.violations.is_empty()
    }

    pub fn report(&self) -> String {
        if self.passed() {
            return format!(
                "{}: ok ({} steps, {} ns, {} switches, {} kicks)",
                self.scenario, self.steps, self.elapsed, self.switches, self.kicks,
            );
        }
        format!(
            "{}: FAILED after {} steps\n  {}",
            self.scenario,
            self.steps,
            self.violations.join("\n  "),
        )
    }
}

pub fn run(scenario: Scenario, choices: &mut ChoiceStream) -> Outcome {
    let name = scenario.name;
    let max_steps = scenario.max_steps;
    let queues = build_queues(&scenario);
    let mut vm = Vm::new(scenario, &queues);

    loop {
        let steps = vm.enabled();
        if steps.is_empty() {
            break;
        }
        if vm.steps >= max_steps {
            vm.violate(format!(
                "non-termination: still {} step(s) enabled after {max_steps} steps",
                steps.len(),
            ));
            break;
        }
        let actors: Vec<Option<usize>> = steps.iter().map(actor).collect();
        let choice = choices.choose_step(&actors);
        vm.execute(steps[choice], choices);
        vm.reap_released();
        vm.collect_dead_processes();
        invariants::check_all(&mut vm);
        // Stop at the first violation: everything after it is a consequence,
        // and a shrunk repro of a consequence is a waste of a regression slot.
        if vm.failed() {
            break;
        }
    }

    if !vm.failed() {
        invariants::check_final(&mut vm);
    }

    let (switches, kicks) = vm.hw.with(|s| (s.switches, s.kicks));
    let outcome = Outcome {
        scenario: name,
        steps: vm.steps,
        violations: vm.all_violations(),
        decisions: choices.recorded().to_vec(),
        elapsed: vm.clock.0,
        switches,
        kicks,
    };

    if !outcome.passed() {
        // A failed run leaves task values in containers, and a `Task` that is
        // dropped outside `finalize()` panics by design (spec §5.1). Letting
        // that fire here would replace the diagnosis with a drop bomb from
        // the teardown; the run is a dead end either way, so it is abandoned
        // deliberately rather than unwound.
        std::mem::forget(vm);
    }
    outcome
}
