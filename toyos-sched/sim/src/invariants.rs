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

use toyos_sched::fair::{MAX_VRUNTIME_LAG_NS, QUANTUM_NS};
use toyos_sched::invariants::{residents, Container};
use toyos_sched::task::{TaskKey, TaskState};

use crate::vm::{FairEpoch, Vm, IPI_LATENCY_NS, RUN_CHUNK_NS};

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
    let rt_present = note_rt_service(vm);
    check_fairness(vm, rt_present);
    check_share_refcounts(vm);
    check_address_spaces(vm);
    check_boost_windows(vm);
}

/// How many Ready-or-Running tasks each process has right now. Invariants I5
/// and I6 both need it and it is the more expensive half of either.
fn runnable_per_process(vm: &Vm<'_>) -> Vec<u32> {
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
    counted
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

/// Mark every process one of whose tasks is currently in the RT band, whether
/// permanently or on a lend, and report whether the band is occupied at all.
/// Both answers are invariant I5's.
///
/// The per-process mark is checked over *all* containers rather than only over
/// running tasks: a client woken with `WakeCause::boosted` is in the RT band
/// from the moment it is queued, and I5 must stop measuring it before it has
/// run out of band, not after. The machine-wide answer counts only Ready and
/// Running, because a *parked* RT task is consuming nothing.
fn note_rt_service(vm: &mut Vm<'_>) -> bool {
    let mut rt = Vec::new();
    let mut occupied = false;
    for cpu in 0..vm.scenario.cpus {
        let sched = &vm.cpus[cpu];
        if let Some(task) = sched.running() {
            if task.rt().is_rt() {
                rt.push(task.key());
                occupied = true;
            }
        }
        for task in sched.rq().tasks() {
            if task.rt().is_rt() {
                rt.push(task.key());
                occupied = true;
            }
        }
        for (key, _, _) in sched.parked() {
            if sched.parked_task(key).is_some_and(|t| t.rt().is_rt()) {
                rt.push(key);
            }
        }
    }
    for key in rt {
        if let Some(process) = vm.process_of(key) {
            vm.procs[process].rt_service = true;
        }
    }
    occupied
}

/// I5: **equal shares receive equal service, to within the granularity the
/// policy chooses** (spec §9.1, §10.5).
///
/// Fairness is a statement about service, so this measures service — the
/// nanoseconds the virtual CPUs actually delivered to each process — and not
/// the vruntime bookkeeping that is supposed to produce it. Checking the
/// bookkeeping against itself is how an instrument stops measuring: a lag that
/// `ShareState::leave_runnable` clamps on the way in satisfies `|lag| ≤ 50 ms`
/// no matter what the scheduler did with the CPU.
///
/// **The window.** Fairness owes nothing across a block: a process with no
/// runnable thread is not being starved, it is waiting. Nor on an unsaturated
/// machine: a CPU with nothing on it is denying nobody. Nor to a process with
/// fewer runnable threads than its even share of the CPUs, which is limited by
/// its own thread count and not by the scheduler. Nor while the RT band is
/// occupied, because the RT band exists to be unfair and invariant I4 is what
/// bounds it. So service is compared over a *contention window* — a maximal
/// interval during which the same set of fair-band processes was continuously
/// runnable, every CPU had a task loaded, every member could absorb its share,
/// and the RT band was empty — and the comparison restarts the moment any of
/// those stops holding. In a workload where everyone blocks the windows are
/// short and this says little; in `fairness_storm`, where nothing blocks and no
/// RT exists, the window is the whole run and it says everything.
///
/// **The bound is derived, and it does not move.** Two terms, both of them a
/// granularity the policy picked:
///
/// * `lag_spread` — the stored lags of the contending shares. A share that
///   parked 50 ms behind the frontier is *entitled* to that much catch-up
///   (§9.1), so the clamp and the intended service difference are the same
///   number. Asserted against `MAX_VRUNTIME_LAG_NS` here rather than assumed,
///   because the bound is only worth what the clamp is worth.
/// * `(runnable threads + 1) × (QUANTUM + max KernelSection + 2 × RUN_CHUNK)` —
///   the fair band is keyed by the vruntime a task had *when it was inserted*
///   (spec §9.2), so a process with T threads carries up to T−1 of its own
///   dispatches' worth of stale keys and can be picked that many times over
///   before its slowest thread comes up. Both sides carry it and the leader is
///   spending one more quantum on top, hence `ΣT_i + 1`. The kernel-section and
///   chunk terms are I9's, for I9's reason: a preempt-off section overruns the
///   quantum it started in, and the model observes the expiry one chunk late.
///
/// Earlier drafts of this check widened that expression twice, because the
/// shipped scheduler brushed it — 74 ms against 72, then 109 ms against 108.
/// Widening was the wrong move and the near-misses were the signal: a bound
/// calibrated to what the code already does cannot detect the code getting
/// worse. The bound is therefore back to the derived form, and where the shipped
/// scheduler does not meet it, that is recorded as a gap rather than absorbed
/// (`scenarios::FAIRNESS_SAMPLE`, which has the measurements and names the four
/// widths of ten where the standard is currently crossed).
///
/// **What reds a run** is `max(derived bound, recorded allowance)`, so a
/// scenario with a sample is gated on not regressing against it. The allowance
/// never hides the standard: `Vm::fair_over_bound` records every crossing of the
/// derived bound whatever the allowance permits, and the sweep prints it.
fn check_fairness(vm: &mut Vm<'_>, rt_present: bool) {
    let runnable = runnable_per_process(vm);
    let saturated = (0..vm.scenario.cpus).all(|cpu| vm.cpus[cpu].running().is_some());
    let mut members: Vec<usize> = if rt_present || !saturated {
        Vec::new()
    } else {
        (0..vm.procs.len())
            .filter(|&p| runnable[p] > 0 && !vm.procs[p].rt_service)
            .collect()
    };
    // A process cannot run on more CPUs than it has runnable threads, so one
    // with fewer than its even share of the machine is limited by its own thread
    // count and not by the scheduler. Measuring it would be measuring the
    // workload: at 16 CPUs it is what the late windows of `fairness_storm` are,
    // once enough threads have exited that the wide process is the only one that
    // can still fill the machine.
    if members
        .iter()
        .any(|&p| runnable[p] as usize * members.len() < vm.scenario.cpus)
    {
        members.clear();
    }

    if members != vm.fair_epoch.members {
        vm.fair_epoch = FairEpoch {
            members,
            base: vm.service_ns.clone(),
            threads: 0,
            lag_spread: 0,
        };
        return;
    }
    if members.len() < 2 {
        return;
    }

    let mut lag_low = i64::MAX;
    let mut lag_high = i64::MIN;
    let mut over_clamp = Vec::new();
    for &process in &members {
        for share in &vm.procs[process].shares {
            let lag = share.lag();
            if lag.unsigned_abs() > MAX_VRUNTIME_LAG_NS {
                over_clamp.push(format!(
                    "I5: {}'s stored lag is {lag} ns, past the ±{MAX_VRUNTIME_LAG_NS} ns clamp \
                     the service bound is derived from",
                    vm.procs[process].name,
                ));
            }
            lag_low = lag_low.min(lag);
            lag_high = lag_high.max(lag);
        }
    }
    for problem in over_clamp {
        vm.violate(problem);
    }

    let threads: u32 = members.iter().map(|&p| runnable[p]).sum();
    vm.fair_epoch.threads = vm.fair_epoch.threads.max(threads);
    vm.fair_epoch.lag_spread = vm
        .fair_epoch
        .lag_spread
        .max(lag_high.saturating_sub(lag_low).unsigned_abs());

    let bound = vm.fair_epoch.lag_spread
        + (vm.fair_epoch.threads as u64 + 1)
            * (QUANTUM_NS + vm.max_kernel_section() + 2 * RUN_CHUNK_NS);
    let served: Vec<u64> = members
        .iter()
        .map(|&p| vm.service_ns[p] - vm.fair_epoch.base[p])
        .collect();
    let (low, high) = (
        served.iter().copied().min().unwrap_or(0),
        served.iter().copied().max().unwrap_or(0),
    );
    let spread = high - low;
    if spread > vm.fair_spread {
        vm.fair_spread = spread;
        vm.fair_bound = bound;
    }
    if spread > bound {
        vm.fair_over_bound = vm.fair_over_bound.max(spread);
    }
    // The ceiling is the bound, except where a recorded sample says the shipped
    // scheduler does not meet it (`scenarios::FAIRNESS_SAMPLE`). There the run is
    // gated on not getting *worse*, and the gap between the two is reported by
    // `Outcome::fair_over_bound` on every run rather than left in a document.
    let ceiling = bound.max(vm.scenario.fair_allowance_ns);
    if spread > ceiling {
        let detail: Vec<String> = members
            .iter()
            .zip(&served)
            .map(|(&p, ns)| format!("{}={ns}", vm.procs[p].name))
            .collect();
        vm.violate(format!(
            "I5: {spread} ns of service separates equal shares over one contention \
             window (bound {bound} ns, recorded allowance {} ns): {}",
            vm.scenario.fair_allowance_ns,
            detail.join(" "),
        ));
    }
}

/// I6: `FairShare.runnable_threads` equals the actual Ready+Running count of
/// that share. The refcount and the containers are driven by the same linear
/// moves, so a drift means a transition forgot one of the two.
fn check_share_refcounts(vm: &mut Vm<'_>) {
    let counted = runnable_per_process(vm);
    let mut problems = Vec::new();
    for (process, expected) in counted.iter().enumerate() {
        // A sum, because a process holds one share under spec §9.1 and one per
        // thread under the `PerThread` negative gate. With one share this is
        // the single `runnable_threads()` read it has always been.
        let actual: u32 = vm.procs[process]
            .shares
            .iter()
            .map(|share| share.runnable_threads())
            .sum();
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

/// I9: **one lend buys at most one quantum of running time at the borrowed
/// priority** (spec §8.5).
///
/// That sentence is now tested directly, against `Vm::boosted_run`'s cumulative
/// *running* residency per lend, rather than by comparing a running task's
/// `until` to the clock. The old form could not survive `RtState::arm`: a
/// re-armed `until` is by construction fresh, so the check passed for the same
/// reason it stopped measuring anything — the same shape as gate A's four
/// instrument defects, and the reason `old_park_kept_the_lend` exists as a
/// standing negative gate rather than a comment.
///
/// Queue time is deliberately outside the bound (§8.5: waiting holds nothing),
/// which is why the accumulator only advances while the task is `Running`.
fn check_boost_windows(vm: &mut Vm<'_>) {
    // One quantum is the grant. The slack on top is measurement, not licence:
    // a preempt-off section can overrun the quantum it started inside, and the
    // model advances the clock in chunks, so the quantum's expiry is observed
    // one chunk late and the pass that acts on it lands a chunk after that.
    let bound = toyos_sched::fair::QUANTUM_NS + vm.max_kernel_section() + 2 * RUN_CHUNK_NS;
    let problems: Vec<String> = vm
        .boosted_run
        .iter()
        .filter(|(_, (_, ns))| *ns > bound)
        .map(|(key, (lends, ns))| {
            format!(
                "I9: {key:?} has run {ns} ns at a borrowed priority on lend #{lends} \
                 (bound {bound} ns) — one lend must buy at most one quantum",
            )
        })
        .collect();
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
