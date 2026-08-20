//! The step chooser and the trace recorder — spec §10.3.
//!
//! One loop: compute the enabled steps, ask the [`ChoiceStream`] which one to
//! take, take it, re-check every invariant. A run is completely determined by
//! its decisions, so a failure is a *value* — a list of integers — that can be
//! replayed, shrunk and committed as a regression.

use toyos_sched::cpu::PassCostReport;
use toyos_sched::hw::CpuId;

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
        | Step::BlockPass(cpu)
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
    /// Parks that had their `Blocked` word claimed before the park itself ran
    /// (spec §8.1's residual window). Reported so a test can assert the window
    /// was actually executed rather than merely reasoned about.
    pub pre_park_claims: u64,
    /// Blocks that ended in `Commit::Killed` — a retire that landed inside the
    /// registration window (spec §6.3, §7.6). Reported for the same reason.
    pub killed_at_park: u64,
    /// Invariant I14's measurement: the longest a retire went unfinalized, and
    /// the bound in force. A number as well as a verdict, because the kernel's
    /// `retire_task` states the same property with a wall clock and a panic, and
    /// how much of that budget the protocol spends is what says whether the wall
    /// clock is a backstop or a coin flip.
    pub retire_latency: u64,
    pub retire_bound: u64,
    /// The longest one CPU's unwind gate held every other CPU still — see
    /// [`crate::vm::Vm::unwind_gate_ns`]. Reported rather than only asserted,
    /// because it is a statement about the *model's* fidelity and not about the
    /// scheduler: a gate wider than one chunk means the explorer was not free
    /// to interleave over the teardown window I14 is measured across.
    pub unwind_gate_ns: u64,
    /// Invariant I5's measurement: the widest service spread seen over one
    /// contention window, and the bound in force when it was seen. A number
    /// rather than a verdict, because spec §11 Stage 9 compares a per-CPU
    /// frontier against the global one and "both passed" is not a comparison.
    pub fair_spread: u64,
    pub fair_bound: u64,
    /// Worst spread past the *derived* bound, even where the recorded sample
    /// allowed the run to pass. Zero means the run met the standard.
    pub fair_over_bound: u64,
    /// Virtual nanoseconds I5 had a comparison open for. Its *reach*, which is a
    /// separate question from its verdict and one that four different conditions
    /// can silently shrink; see [`crate::vm::Vm::fair_covered_ns`].
    pub fair_covered_ns: u64,
    /// Invariant I13's measurement, in the same three roles: the widest service
    /// spread between threads of one share, the bound in force when it was
    /// seen, and any crossing of the derived bound the allowance let pass.
    pub thread_spread: u64,
    pub thread_bound: u64,
    pub thread_over_bound: u64,
    /// Virtual nanoseconds I13 had a comparison open for. Its *reach*, which is
    /// a separate question from its verdict and one a change to the pick or the
    /// balance can silently shrink; see [`crate::vm::Vm::thread_covered_ns`].
    pub thread_covered_ns: u64,
    /// What each CPU's passes cost, as the core's `feature = "check"` recorder
    /// measured them — the on-target instrument, driven here by the scenario's
    /// modelled `pass_cost_ns`.
    ///
    /// Reported rather than judged, and for the same reason the kernel does not
    /// judge it either: a pass's elapsed time composes the scheduler's own work
    /// with whatever the world underneath it did. What the simulator can say is
    /// that the recorder counts what it is given, which is
    /// [`crate::scenarios::overlong_pass`]'s whole job.
    pub pass_costs: Vec<PassCostReport>,
}

impl Outcome {
    pub fn passed(&self) -> bool {
        self.violations.is_empty()
    }

    pub fn report(&self) -> String {
        if self.passed() {
            return format!(
                "{}: ok ({} steps, {} ns, {} switches, {} kicks, I5 spread {}/{} ns, \
                 I13 spread {}/{} ns, I14 retire {}/{} ns)",
                self.scenario,
                self.steps,
                self.elapsed,
                self.switches,
                self.kicks,
                self.fair_spread,
                self.fair_bound,
                self.thread_spread,
                self.thread_bound,
                self.retire_latency,
                self.retire_bound,
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
    explore(&mut vm, choices, max_steps);
    let outcome = outcome_of(name, &vm, choices);
    if !outcome.passed() {
        abandon(vm);
    }
    outcome
}

/// Run a scenario and report an *abort* out of the core as the verdict it is.
///
/// Only [`crate::scenarios::old_preemptible_window`] needs this. Everything
/// else the simulator finds is an invariant walk's verdict, recorded and
/// returned; what a pass landing inside the registration window provokes is
/// the core's own `check_cpu` assertion, which unwinds instead. A `Vm` dropped
/// during that unwind fires its linear types' drop bombs on top of it and turns
/// the diagnosis into a non-unwinding abort, so it is abandoned here exactly as
/// a failed run is.
pub fn run_catching(scenario: Scenario, choices: &mut ChoiceStream) -> Result<Outcome, String> {
    let name = scenario.name;
    let max_steps = scenario.max_steps;
    let queues = build_queues(&scenario);
    let mut vm = Vm::new(scenario, &queues);

    // The panic is the expected result, so it is not also news. The hook is
    // restored before returning either way.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        explore(&mut vm, choices, max_steps)
    }));
    std::panic::set_hook(hook);

    match caught {
        Ok(()) => {
            let outcome = outcome_of(name, &vm, choices);
            if !outcome.passed() {
                abandon(vm);
            }
            Ok(outcome)
        }
        Err(payload) => {
            abandon(vm);
            Err(panic_message(payload))
        }
    }
}

fn explore(vm: &mut Vm<'_>, choices: &mut ChoiceStream, max_steps: usize) {
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
        invariants::check_all(vm);
        // Stop at the first violation: everything after it is a consequence,
        // and a shrunk repro of a consequence is a waste of a regression slot.
        if vm.failed() {
            break;
        }
    }

    if !vm.failed() {
        invariants::check_final(vm);
    }
}

fn outcome_of(scenario: &'static str, vm: &Vm<'_>, choices: &ChoiceStream) -> Outcome {
    let (switches, kicks) = vm.hw.with(|s| (s.switches, s.kicks));
    Outcome {
        scenario,
        steps: vm.steps,
        violations: vm.all_violations(),
        decisions: choices.recorded().to_vec(),
        elapsed: vm.clock.0,
        switches,
        kicks,
        pre_park_claims: vm.pre_park_claims,
        killed_at_park: vm.killed_at_park,
        retire_latency: vm.retire_latency,
        retire_bound: vm.retire_bound,
        unwind_gate_ns: vm.unwind_gate_ns,
        fair_spread: vm.fair_spread,
        fair_bound: vm.fair_bound,
        fair_over_bound: vm.fair_over_bound,
        fair_covered_ns: vm.fair_covered_ns,
        thread_spread: vm.thread_spread,
        thread_bound: vm.thread_bound,
        thread_over_bound: vm.thread_over_bound,
        thread_covered_ns: vm.thread_covered_ns,
        pass_costs: (0..vm.handles.len())
            .map(|cpu| {
                let cpu = CpuId(cpu as u32);
                vm.handles.get(cpu).pass_costs().report(cpu)
            })
            .collect(),
    }
}

/// A failed run leaves task values in containers, and a `Task` dropped outside
/// `finalize()` panics by design (spec §5.1). Letting that fire would replace
/// the diagnosis with a drop bomb from the teardown; the run is a dead end
/// either way, so it is abandoned deliberately rather than unwound.
fn abandon(vm: Vm<'_>) {
    std::mem::forget(vm);
}

/// Taken by value and downcast through the `Box`: `&Box<dyn Any + Send>`
/// unsize-coerces to `&dyn Any` whose concrete type is the *box*, so the
/// by-reference spelling of this silently matches nothing.
fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    let payload = match payload.downcast::<String>() {
        Ok(message) => return *message,
        Err(payload) => payload,
    };
    match payload.downcast::<&'static str>() {
        Ok(message) => (*message).to_string(),
        Err(_) => "the core aborted with a non-string panic payload".to_string(),
    }
}
