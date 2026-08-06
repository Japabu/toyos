//! The global walks — spec §10.5, invariants I1–I13.
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

use std::collections::{BTreeMap, BTreeSet};
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

/// How long a retire may take to reach `Hw::release` (invariant I14).
///
/// One quantum is what a *running* target is allowed before its next safe
/// point — spec §7.6's "bounded by the quantum" — and the other three terms
/// are I4's: the kick's delivery bound, the preempt-off section the target may
/// be inside, and the granularity of an execution step. Everything else the
/// protocol does is a message the target consumes at the start of that pass.
///
/// Derived, not recorded, and deliberately *not* the kernel's 1 s wall clock:
/// `retire_task`'s deadline is a hundred times this, so a scenario that holds
/// this bound says the wall clock had two orders of magnitude of headroom.
fn retire_latency_bound(max_kernel_section: u64) -> u64 {
    QUANTUM_NS + IPI_LATENCY_NS + max_kernel_section + 2 * RUN_CHUNK_NS
}

pub fn check_all(vm: &mut Vm<'_>) {
    check_single_ownership(vm);
    check_sleeping_cpus(vm);
    check_timers(vm);
    check_rt_latency(vm);
    check_retires(vm);
    let rt_present = note_rt_service(vm);
    check_fairness(vm, rt_present);
    check_share_refcounts(vm);
    check_address_spaces(vm);
    check_boost_windows(vm);
}

/// What one walk of the containers has to yield for both fairness invariants:
/// the per-process runnable counts I5's member test needs, the same counts
/// broken down per CPU for I13's balance test, and the set of individual
/// threads that are owed service.
///
/// A task in transit between CPUs is in none of the three. It is in no
/// container — invariant I1 requires its state word to say so — it has left its
/// share's refcount, and it is exactly a thread whose competition is changing,
/// which is the one thing I13 declines to measure across.
fn runnable_now(vm: &Vm<'_>) -> (Vec<u32>, Vec<u32>, BTreeSet<TaskKey>) {
    let cpus = vm.scenario.cpus;
    let mut counted = vec![0u32; vm.procs.len()];
    let mut per_cpu = vec![0u32; vm.procs.len() * cpus];
    let mut threads = BTreeSet::new();
    for cpu in 0..cpus {
        for (key, container) in residents(&vm.cpus[cpu]) {
            if !matches!(container, Container::Running | Container::Ready) {
                continue;
            }
            threads.insert(key);
            if let Some(process) = vm.process_of(key) {
                counted[process] += 1;
                per_cpu[process * cpus + cpu] += 1;
            }
        }
    }
    (counted, per_cpu, threads)
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

/// I14: a retire is prompt, and the balance path does not undo that.
///
/// Two halves of one property, because the protocol's promptness rests on two
/// different things. **A killed task is never migrated**: `InTransit` is the
/// one state whose reap is not backed by an interrupt — the destination's
/// adopt carries `Urgency::Normal`, which by design sends no IPI to a busy CPU
/// — so a CPU that hands on a task it knows is dead trades a reap it could do
/// in this pass for a wait on another CPU's next voluntary one. **And a retire
/// completes within [`retire_latency_bound`]**, which is the statement the
/// kernel's `retire_task` makes with a wall clock and a panic.
///
/// The first half is what `scenarios::old_migrate_kept_the_corpse` proves has
/// teeth; the second is a bound in the shape of I4's, and it is what a future
/// change that lengthens the retire path would go red on.
fn check_retires(vm: &mut Vm<'_>) {
    let mut problems = Vec::new();

    let migrated: Vec<(TaskKey, u32)> = vm.hw.with(|s| {
        let fresh = s.trace[vm.trace_cursor..]
            .iter()
            .filter_map(|ev| match ev.kind {
                toyos_sched::hw::TraceKind::Migrate { task, to } => Some((task, to.0)),
                _ => None,
            })
            .collect();
        vm.trace_cursor = s.trace.len();
        fresh
    });
    for (key, to) in migrated {
        if vm.killed.contains_key(&key) {
            problems.push(format!(
                "I14: {key:?} was killed and then migrated to cpu{to} — a task in transit is \
                 reaped only by the adopt that carries it, and that adopt kicks nobody",
            ));
        }
    }

    let bound = retire_latency_bound(vm.max_kernel_section());
    vm.retire_bound = bound;
    let mut done = Vec::new();
    for (&key, &at) in &vm.killed {
        if vm.live.contains(&key) {
            let elapsed = vm.clock.since(at);
            vm.retire_latency = vm.retire_latency.max(elapsed);
            if elapsed > bound {
                problems.push(format!(
                    "I14: {key:?} was retired {elapsed} ns ago and is still {:?} (bound {bound} ns)",
                    vm.shared[&key].state(),
                ));
            }
        } else {
            done.push(key);
        }
    }
    for key in done {
        let at = vm.killed.remove(&key).expect("came from the map");
        vm.retire_latency = vm.retire_latency.max(vm.clock.since(at));
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
        let deadline = vm.cpus[cpu].parked().filter_map(|p| p.deadline()).min();
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
        for parked in sched.parked() {
            if parked.is_rt() {
                rt.push(parked.key());
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
    // Cleared here and re-established only by a comparison that actually
    // happened, so `Vm::thread_covered_ns` counts the reach I13 has and not the
    // reach it would have if every window stayed open.
    vm.thread_window_open = false;
    let (runnable, per_cpu, live_threads) = runnable_now(vm);
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

    // I13's narrowing: every CPU must carry the same number of each member's
    // runnable threads. Two siblings on differently composed CPUs are limited by
    // where they were placed and not by the order the fair band picks them in,
    // and the policy makes no per-thread placement promise to hold them to.
    // Measured rather than assumed, because it is what `fairness_storm`'s
    // "balanced by construction" is worth once threads start exiting: at two
    // CPUs the shipped scheduler separates two trio threads by 49 ms with one of
    // them sharing a CPU with a solo thread that is catching up, which is I5's
    // entitlement being honoured and not a sibling being cheated.
    let balanced = members.iter().all(|&process| {
        let row = &per_cpu[process * vm.scenario.cpus..(process + 1) * vm.scenario.cpus];
        row.iter().min() == row.iter().max()
    });
    let threads: u32 = members.iter().map(|&p| runnable[p]).sum();
    // What one CPU's fair band holds. Read off CPU 0 rather than divided out of
    // the machine-wide total, so it is the quantity itself and not an average
    // that happens to equal it; `balanced` is what makes every CPU's the same.
    let rivals: u32 = members.iter().map(|&p| per_cpu[p * vm.scenario.cpus]).sum();

    if members != vm.fair_epoch.members {
        vm.fair_epoch = FairEpoch {
            members,
            base: vm.service_ns.clone(),
            threads: 0,
            lag_spread: 0,
            thread_base: BTreeMap::new(),
            thread_rivals: 0,
        };
        open_thread_window(vm, balanced, &live_threads);
        return;
    }
    if !balanced {
        vm.fair_epoch.thread_base.clear();
    } else if vm.fair_epoch.thread_base.is_empty() {
        open_thread_window(vm, balanced, &live_threads);
    } else {
        // A thread that stopped being runnable is owed nothing further, so it
        // drops out of the comparison for the rest of this window rather than
        // closing it: a share whose threads exit one by one is still a share
        // whose *remaining* threads must be sharing evenly.
        vm.fair_epoch
            .thread_base
            .retain(|key, _| live_threads.contains(key));
        vm.fair_epoch.thread_rivals = vm.fair_epoch.thread_rivals.max(rivals);
        check_thread_service(vm, &members);
    }

    if members.len() < 2 {
        return;
    }
    vm.fair_epoch.threads = vm.fair_epoch.threads.max(threads);

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

/// (Re-)baseline invariant I13's measurement at this instant: every runnable
/// thread of a member, with the service it has had so far. A no-op while the
/// members' threads are unevenly spread across the CPUs, because that is
/// exactly the interval the comparison declines to make.
fn open_thread_window(vm: &mut Vm<'_>, balanced: bool, live_threads: &BTreeSet<TaskKey>) {
    if !balanced {
        return;
    }
    let base: BTreeMap<TaskKey, u64> = vm
        .fair_epoch
        .members
        .iter()
        .flat_map(|&process| vm.procs[process].live.iter().copied())
        .filter(|key| live_threads.contains(key))
        .map(|key| (key, vm.thread_service.get(&key).copied().unwrap_or(0)))
        .collect();
    vm.fair_epoch.thread_base = base;
    vm.fair_epoch.thread_rivals = 0;
}

/// I13: **threads of one share receive equal service**, measured over the same
/// contention windows invariant I5 measures processes over.
///
/// I5 is structurally blind to this. A fair share is per *process* (spec §9.1),
/// so every thread of a process charges one pot and `service_ns` adds all of
/// them together: a share that runs one thread flat out and never dispatches
/// its siblings delivers exactly the per-process total that a share
/// round-robining them delivers, and I5 reports a perfectly even split while a
/// thread never runs. What prevents that today is the fair band's
/// insertion-sequence tie-break (`queue.rs`, spec §9.2) — and until this check
/// existed, nothing measured it.
///
/// **The window is I5's**, taken from the same `fair_epoch` and opened and
/// closed by the same rules — the runnable set changing, a CPU idling, a member
/// with fewer runnable threads than its even share of the CPUs, an occupied RT
/// band. Two definitions of one word would be a defect rather than a design.
/// I13's is that interval with three changes, two of them narrowings:
///
/// * **One further narrowing of its own: every CPU must carry the same number
///   of each member's runnable threads.** Two siblings on differently composed
///   CPUs are limited by where they were placed, and the policy makes no
///   per-thread placement promise to hold them to; I13's window re-baselines
///   the instant that stops holding, so it is a sub-interval of I5's rather
///   than a second notion of contention. This is not a hypothetical: at two
///   CPUs the shipped scheduler separates two `trio` threads by 49 ms with one
///   of them sharing a CPU with a `solo` thread that is catching up — I5's
///   entitlement being honoured, not a sibling being cheated.
/// * The measured set is fixed when the window opens and only ever shrinks. A
///   thread that blocks or exits mid-window is owed nothing further and leaves
///   the comparison — it does not close the window, because the siblings still
///   runnable are still owed an even split.
/// * A window with a *single* member is still measured, which is the one
///   relaxation. I5 needs two processes before it has a spread at all; one
///   process with two threads is already a comparison.
///
/// **The bound is derived, and it does not move.** It is I5's bound with the
/// lag term deleted, and the deletion is the whole content of it:
///
/// * `(ΣT_i + 1) × (QUANTUM + max KernelSection + 2 × RUN_CHUNK)` — I5's own
///   staleness term, unchanged and computed from the same running maximum. The
///   fair band is keyed by the vruntime a thread held when it was *inserted*,
///   and a dispatched thread is re-inserted with its share's pot as it stands
///   after the charge — strictly above the key it was holding. So every other
///   ready thread in the window, sibling or not, can be picked at most once
///   ahead of a waiting thread on the strength of a key that was already stale
///   when the wait began, and the leader spends one more dispatch on top. It is
///   `ΣT_i` and not the share's own thread count because a thread waits behind
///   the *whole* fair band on its CPU: an earlier draft of this check counted
///   only siblings, which is a derivation that assumes the rest of the machine
///   away, and the shipped scheduler crossed it at every width above one. The
///   running maximum is I13's own (`thread_threads`) because I13's window is
///   the shorter of the two.
/// * **No lag term.** I5 carries `lag_spread` because two different shares may
///   hold stored lags up to ±[`MAX_VRUNTIME_LAG_NS`] apart and are entitled to
///   that much catch-up. [`toyos_sched::fair::ShareState`] holds one vruntime
///   and one lag for every thread of a process, so the intra-share lag spread
///   is identically zero — there is no per-thread entitlement to be behind.
///   That absence is exactly why a per-share check can be tighter than I5, and
///   why it can see what I5 cannot.
///
/// **What it does not cover is placement.** The insertion-order argument is
/// about one run queue, and two threads of one share on differently loaded CPUs
/// sit in two of them; nothing in the policy equalizes those, and a check that
/// asserted it would be measuring the workload. `scenarios::fairness_storm`
/// hands every CPU the identical mix by construction (its own doc says why),
/// which is what makes the number reported there a statement about the
/// ordering.
///
/// **What reds a run** is `max(derived bound, recorded allowance)`, and
/// `Vm::thread_over_bound` records every crossing of the derived bound whatever
/// the allowance permits. I5's recording pattern, for I5's reason: an allowance
/// that can quietly become the standard is a gate that has stopped measuring.
fn check_thread_service(vm: &mut Vm<'_>, members: &[usize]) {
    let bound = (u64::from(vm.fair_epoch.thread_rivals) + 1)
        * (QUANTUM_NS + vm.max_kernel_section() + 2 * RUN_CHUNK_NS);
    // Measured without allocating per member: this runs after every step of
    // every scenario, and the detail line is built only where one is owed.
    let mut measured: Vec<(usize, u64)> = Vec::new();
    for &process in members {
        let (mut low, mut high, mut counted) = (u64::MAX, 0u64, 0usize);
        for key in &vm.procs[process].live {
            let Some(base) = vm.fair_epoch.thread_base.get(key) else {
                continue;
            };
            let served = vm.thread_service.get(key).copied().unwrap_or(0) - base;
            low = low.min(served);
            high = high.max(served);
            counted += 1;
        }
        if counted < 2 {
            continue;
        }
        measured.push((process, high - low));
    }
    vm.thread_window_open = !measured.is_empty();

    let mut problems = Vec::new();
    for (process, spread) in measured {
        if spread > vm.thread_spread {
            vm.thread_spread = spread;
            vm.thread_bound = bound;
        }
        if spread > bound {
            vm.thread_over_bound = vm.thread_over_bound.max(spread);
        }
        if spread <= bound.max(vm.scenario.thread_allowance_ns) {
            continue;
        }
        let detail: Vec<String> = vm.procs[process]
            .live
            .iter()
            .filter_map(|key| {
                let base = vm.fair_epoch.thread_base.get(key)?;
                let served = vm.thread_service.get(key).copied().unwrap_or(0) - base;
                // Which run queue each thread is in, because that is the first
                // question a per-thread spread raises and the answer is one
                // scan on a path that has already failed.
                let at = (0..vm.scenario.cpus)
                    .find(|&cpu| residents(&vm.cpus[cpu]).any(|(k, _)| k == *key))
                    .map_or_else(|| "?".to_string(), |cpu| cpu.to_string());
                Some(format!("{}@cpu{at}={served}", key.0))
            })
            .collect();
        problems.push(format!(
            "I13: {spread} ns of service separates threads of {}'s one share over one \
             contention window (bound {bound} ns, recorded allowance {} ns): {}",
            vm.procs[process].name,
            vm.scenario.thread_allowance_ns,
            detail.join(" "),
        ));
    }
    for problem in problems {
        vm.violate(problem);
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
