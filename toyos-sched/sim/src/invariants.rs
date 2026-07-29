//! The global walks — spec §10.5, invariants I1–I12.
//!
//! These run after **every** step. They are the reason the simulator exists:
//! the linear types make most of these states unrepresentable in *scheduler*
//! code, but the protocol above them — who posts what to whom, and when — is
//! not something the compiler can check, and the old scheduler's failures all
//! lived exactly there.
//!
//! Division of labour with loom is stated in the spec and honoured here: loom
//! owns the primitives (mailbox linearizability, doorbell edges, the ticket
//! CAS protocol, weak memory); this file owns the protocol above them and does
//! not model memory ordering at all.

use std::collections::BTreeMap;
use std::sync::Arc as StdArc;

use toyos_sched::invariants::{residents, Container};
use toyos_sched::task::{TaskKey, TaskState};

use crate::vm::{Vm, IPI_LATENCY_NS, RUN_CHUNK_NS};

/// How long a CPU may keep running a normal task while an RT task is ready on
/// it (invariant I4): the interrupt's own delivery bound, plus the
/// preempt-off section it may have to wait out, plus the granularity of one
/// execution step. Measured in the CPU's *own* busy time, so another CPU's
/// progress cannot inflate it.
fn rt_latency_bound(max_kernel_section: u64) -> u64 {
    IPI_LATENCY_NS + max_kernel_section + 2 * RUN_CHUNK_NS
}

pub fn check_all(vm: &mut Vm<'_>) {
    check_single_ownership(vm);
    check_sleeping_cpus(vm);
    check_timers(vm);
    check_rt_latency(vm);
    check_share_refcounts(vm);
    check_address_spaces(vm);
    check_boost_windows(vm);
}

/// I1: every live task is in exactly one container system-wide, and its state
/// word agrees with where it is.
///
/// This is what catches the ported old steal: a task carried on an idle CPU's
/// stack is in no container at all, and one installed into the thief's queue
/// has a word that still names the victim.
fn check_single_ownership(vm: &mut Vm<'_>) {
    let mut seen: BTreeMap<TaskKey, (usize, Container)> = BTreeMap::new();
    let mut problems = Vec::new();

    for cpu in 0..vm.scenario.cpus {
        for (key, container) in residents(&vm.cpus[cpu]) {
            if let Some((other, other_container)) = seen.insert(key, (cpu, container)) {
                problems.push(format!(
                    "I1: {key:?} is in two places at once — cpu{other} {other_container:?} and \
                     cpu{cpu} {container:?}",
                ));
            }
            let state = vm.shared[&key].state();
            let agrees = match (container, state) {
                (Container::Running, TaskState::Running(c))
                | (Container::Ready, TaskState::Ready(c))
                | (Container::Parked, TaskState::Blocked(c))
                | (Container::Parked, TaskState::WakeQueued(c)) => c.0 as usize == cpu,
                // A task that has registered on a wait queue and not yet parked
                // is still the running value (spec §8.1). Exactly two words are
                // legal there: `Committing`, while its own commit is still
                // owed, and `WakeQueued`, once a remote claim has taken it
                // pre-park — §8.2's `Claim::PrePark`, which posts no message
                // precisely because the waiter has not parked. Both are legal
                // *only* inside that window, which is why this consults the
                // CPU's pending block instead of accepting the words outright:
                // a word that says the task is blocked while its CPU still runs
                // it is otherwise exactly the single-ownership break the
                // pre-`8508b37` blocking shape had.
                (Container::Running, TaskState::Committing(c, _))
                | (Container::Running, TaskState::WakeQueued(c)) => {
                    c.0 as usize == cpu && vm.blocking[cpu].is_some()
                }
                (Container::Zombie, TaskState::Dead) => true,
                _ => false,
            };
            if !agrees {
                problems.push(format!(
                    "I1: {key:?} sits in cpu{cpu} {container:?} but its word says {state:?}",
                ));
            }
        }
    }

    // A live task that is in no container must be inside an unconsumed
    // message — which the word records as `InTransit`. Anything else has been
    // dropped on the floor.
    for &key in &vm.live {
        if seen.contains_key(&key) {
            continue;
        }
        match vm.shared[&key].state() {
            TaskState::InTransit(_) => {}
            state => problems.push(format!(
                "I1: {key:?} is in no container and its word says {state:?}",
            )),
        }
    }

    for problem in problems {
        vm.violate(problem);
    }
}

/// I2: a sleeping CPU has nothing to do — or an IPI is on its way to tell it
/// otherwise. This is B4, the ready task stranded on a halted CPU.
fn check_sleeping_cpus(vm: &mut Vm<'_>) {
    let mut problems = Vec::new();
    let (halted, pending) = vm.hw.with(|s| (s.halted.clone(), s.pending_ipi.clone()));
    for cpu in 0..vm.scenario.cpus {
        if !halted[cpu] || pending[cpu] > 0 {
            continue;
        }
        if !vm.cpus[cpu].rq().is_empty() {
            problems.push(format!(
                "I2: cpu{cpu} halted with {} ready task(s) and no IPI pending",
                vm.cpus[cpu].rq().len(),
            ));
        }
        if !vm.cpus[cpu].mailbox_is_empty() {
            problems.push(format!(
                "I2: cpu{cpu} halted with a non-empty mailbox and no IPI pending",
            ));
        }
    }
    for problem in problems {
        vm.violate(problem);
    }
}

/// I3 / invariant T: the armed deadline is never later than the earliest
/// thing the CPU owes (spec §8.4). Delegated to the core's own checker, which
/// is the same code a kernel `feature="check"` build runs.
fn check_timers(vm: &mut Vm<'_>) {
    for cpu in 0..vm.scenario.cpus {
        let armed = vm.cpus[cpu].armed();
        let quantum = vm.cpus[cpu].running().map(|_| vm.cpus[cpu].quantum_end());
        let deadline = vm.cpus[cpu].parked().filter_map(|(_, at, _)| at).min();
        let earliest = match (quantum, deadline) {
            (Some(q), Some(d)) => Some(q.min(d)),
            (Some(q), None) => Some(q),
            (None, d) => d,
        };
        match (armed, earliest) {
            (Some(armed), Some(due)) if armed > due => vm.violate(format!(
                "I3: cpu{cpu} armed at {armed:?} but owes an event at {due:?}",
            )),
            (None, Some(due)) => vm.violate(format!(
                "I3: cpu{cpu} owes an event at {due:?} with no timer armed",
            )),
            _ => {}
        }
    }
}

/// I4: an RT task must not sit ready while the CPU that owns it keeps running
/// a normal task — B7, the wake that did not preempt.
fn check_rt_latency(vm: &mut Vm<'_>) {
    let bound = rt_latency_bound(vm.max_kernel_section());
    let mut problems = Vec::new();
    for cpu in 0..vm.scenario.cpus {
        let starving = vm.cpus[cpu].rq().has_rt()
            && vm.cpus[cpu]
                .running()
                .is_some_and(|task| !task.rt().is_rt());
        match (starving, vm.rt_pending_since[cpu]) {
            (true, None) => vm.rt_pending_since[cpu] = Some(vm.busy_ns[cpu]),
            (true, Some(since)) => {
                let waited = vm.busy_ns[cpu] - since;
                if waited > bound {
                    problems.push(format!(
                        "I4: cpu{cpu} ran a normal task for {waited} ns with an RT task ready \
                         (bound {bound} ns)",
                    ));
                    vm.rt_pending_since[cpu] = Some(vm.busy_ns[cpu]);
                }
            }
            (false, _) => vm.rt_pending_since[cpu] = None,
        }
    }
    for problem in problems {
        vm.violate(problem);
    }
}

/// I6: `FairShare.runnable_threads` equals the actual Ready+Running count of
/// that share. The refcount and the containers are driven by the same linear
/// moves, so a drift means a transition forgot one of the two.
fn check_share_refcounts(vm: &mut Vm<'_>) {
    let mut counted = vec![0u32; vm.procs.len()];
    for cpu in 0..vm.scenario.cpus {
        for (key, container) in residents(&vm.cpus[cpu]) {
            if !matches!(container, Container::Running | Container::Ready) {
                continue;
            }
            if let Some(process) = vm.process_of(key) {
                counted[process] += 1;
            }
        }
    }
    let mut problems = Vec::new();
    for (process, expected) in counted.iter().enumerate() {
        let actual = vm.procs[process].share.runnable_threads();
        if actual != *expected {
            problems.push(format!(
                "I6: {} has {actual} runnable thread(s) counted but {expected} in queues",
                vm.procs[process].name,
            ));
        }
    }
    for problem in problems {
        vm.violate(problem);
    }
}

/// I8: the mock address space's refcount equals the number of live tasks that
/// reference it, plus the process's own reference while it still holds one.
/// The crash.md detector.
fn check_address_spaces(vm: &mut Vm<'_>) {
    let mut problems = Vec::new();
    for process in 0..vm.procs.len() {
        let Some(space) = vm.procs[process].address_space.as_ref() else {
            continue;
        };
        let expected = vm.procs[process].live.len() + 1;
        // Counted through a borrow, never a clone: a clone would be one more
        // reference and the check would be measuring itself.
        let actual = StdArc::strong_count(space);
        if actual != expected {
            problems.push(format!(
                "I8: {}'s address space has {actual} reference(s), expected {expected}",
                vm.procs[process].name,
            ));
        }
    }
    for problem in problems {
        vm.violate(problem);
    }
}

/// I9: an inherited RT window is a *time* bound, so a boosted task that never
/// blocks still loses the boost — the "spinning boosted client keeps RT
/// forever" hole (spec §8.5).
fn check_boost_windows(vm: &mut Vm<'_>) {
    let now = vm.clock;
    // The window may outlast its expiry by the quantum the task was already
    // granted (spec §8.5: "≤ window + one quantum"), plus the preempt-off
    // section that can delay the pass which would end it, plus one execution
    // step — the granularity at which this model advances its clock, and
    // therefore the latest a quantum timer can be observed to fire.
    // Two steps: one for the granularity at which the quantum timer is
    // observed to fire, one for the pass that follows it.
    let slack = toyos_sched::fair::QUANTUM_NS + vm.max_kernel_section() + 2 * RUN_CHUNK_NS;
    let mut problems = Vec::new();
    for cpu in 0..vm.scenario.cpus {
        let Some(task) = vm.cpus[cpu].running() else {
            continue;
        };
        if let Some(until) = task.rt().inherited {
            if now.since(until) > slack {
                problems.push(format!(
                    "I9: {:?} still holds a boost that expired at {until:?} (now {now:?})",
                    task.key(),
                ));
            }
        }
    }
    for problem in problems {
        vm.violate(problem);
    }
}

/// I7 and I10, checked once at the end of a run rather than after every step:
/// every task finalized exactly once, nothing left queued, and the accounting
/// adds up to the time the CPUs actually spent.
pub fn check_final(vm: &mut Vm<'_>) {
    let mut problems = Vec::new();

    if !vm.live.is_empty() {
        let stuck: Vec<String> = vm
            .live
            .iter()
            .map(|key| {
                let state = vm.shared[key].state();
                // The kill bit is the discriminator between a lost wake and a
                // lost retire — two different bugs with one symptom.
                if vm.shared[key].kill_pending() {
                    format!("{key:?}={state:?} killed")
                } else {
                    format!("{key:?}={state:?}")
                }
            })
            .collect();
        problems.push(format!(
            "I10: the run quiesced with {} task(s) never finalized: {}",
            vm.live.len(),
            stuck.join(", "),
        ));
    }
    for cpu in 0..vm.scenario.cpus {
        if !vm.cpus[cpu].rq().is_empty() {
            problems.push(format!("I10: cpu{cpu} quiesced with a non-empty run queue"));
        }
        if !vm.cpus[cpu].mailbox_is_empty() {
            problems.push(format!("I10: cpu{cpu} quiesced with a non-empty mailbox"));
        }
        if vm.cpus[cpu].running().is_some() {
            problems.push(format!("I10: cpu{cpu} quiesced with a task still running"));
        }
    }
    for (index, queue) in vm.queues.iter().enumerate() {
        if !queue.queue.is_empty() {
            problems.push(format!(
                "I10: queue{index} quiesced with {} registration(s) left behind",
                queue.queue.len(),
            ));
        }
    }

    let accounted: u64 = vm.finalized.iter().map(|(_, acct)| acct.cpu_ns).sum();
    let executed: u64 = vm.busy_ns.iter().sum();
    if accounted != executed {
        problems.push(format!(
            "I7: tasks accounted {accounted} ns of CPU but the CPUs executed {executed} ns",
        ));
    }

    for problem in problems {
        vm.violate(problem);
    }
}
