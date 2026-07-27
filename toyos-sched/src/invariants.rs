//! The checks that run *inside* the core — spec §10.5's "cheap subset at
//! kernel pass ends".
//!
//! Everything here is local to one CPU, because that is all a pass can see
//! without reaching into another CPU's state — which is exactly what the
//! design forbids. The global walks (I1 single ownership across every
//! container and message, I8 payload refcounts, I7 accounting conservation)
//! belong to the simulator, which is the only world that can hold all CPUs at
//! once; [`residents`] is what it walks this CPU with.
//!
//! `feature = "check"` gates the call in `SchedPass::finish`, not this module:
//! the simulator wants the enumeration unconditionally, and a check that is
//! compiled but never called costs nothing.

use crate::cpu::CpuSched;
use crate::hw::Nanos;
use crate::task::{SchedPayload, TaskKey, TaskState};

/// Where a task value lives. One key may appear in exactly one of these,
/// system-wide — the property the linear types make unrepresentable and that
/// the simulator re-checks anyway, because a checker that only tests what the
/// compiler already proved would prove nothing about the protocol.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Container {
    Running,
    Ready,
    Parked,
    Zombie,
}

/// Every task this CPU owns, with the container it sits in.
pub fn residents<X: SchedPayload>(
    cpu: &CpuSched<X>,
) -> impl Iterator<Item = (TaskKey, Container)> + '_ {
    cpu.running()
        .map(|t| (t.key(), Container::Running))
        .into_iter()
        .chain(cpu.rq().keys().map(|k| (k, Container::Ready)))
        .chain(cpu.parked().map(|(k, _, _)| (k, Container::Parked)))
        .chain(cpu.zombie_key().map(|k| (k, Container::Zombie)))
}

/// Container-versus-state-word agreement, plus invariant T.
///
/// This is the fail-fast layer that would have preserved crash.md's evidence:
/// a task whose word says one thing while its value sits somewhere else dies
/// here, at the transition that made it so, instead of being discovered later
/// as a double-drop with the panic handler already recursing.
pub fn check_cpu<X: SchedPayload>(cpu: &CpuSched<X>) {
    let id = cpu.id();

    if let Some(running) = cpu.running() {
        assert_eq!(
            running.shared().state(),
            TaskState::Running(id),
            "running task {:?} disagrees with its state word",
            running.key(),
        );
    }

    for task in cpu.rq().tasks() {
        assert_eq!(
            task.shared().state(),
            TaskState::Ready(id),
            "ready task {:?} disagrees with its state word",
            task.key(),
        );
    }

    for (key, _, _) in cpu.parked() {
        let task = cpu.parked_task(key).expect("key came from the map");
        let state = task.shared().state();
        assert!(
            matches!(state, TaskState::Blocked(c) | TaskState::WakeQueued(c) if c == id),
            "parked task {key:?} disagrees with its state word: {state:?}",
        );
    }

    if let Some(key) = cpu.zombie_key() {
        assert!(
            residents(cpu).filter(|(k, _)| *k == key).count() == 1,
            "zombie {key:?} is also somewhere else",
        );
    }

    check_timer(cpu);
}

/// Invariant T (spec §8.4): outside a pass, the armed deadline is no later
/// than the earliest thing that must happen — the running task's quantum end
/// or the earliest valid parked deadline. And a CPU with nothing loaded and a
/// pending deadline must have the timer armed at all.
pub fn check_timer<X: SchedPayload>(cpu: &CpuSched<X>) {
    let earliest = earliest_event(cpu);
    match (cpu.armed(), earliest) {
        (Some(armed), Some(due)) => assert!(
            armed <= due,
            "invariant T: cpu {:?} armed at {armed:?} but owes an event at {due:?}",
            cpu.id(),
        ),
        (_, None) => {}
        (None, Some(due)) => panic!(
            "invariant T: cpu {:?} has an event at {due:?} and no timer armed",
            cpu.id(),
        ),
    }
}

fn earliest_event<X: SchedPayload>(cpu: &CpuSched<X>) -> Option<Nanos> {
    let quantum = cpu.running().map(|_| cpu.quantum_end());
    let deadline = cpu.parked().filter_map(|(_, deadline, _)| deadline).min();
    match (quantum, deadline) {
        (Some(q), Some(d)) => Some(q.min(d)),
        (Some(q), None) => Some(q),
        (None, d) => d,
    }
}
