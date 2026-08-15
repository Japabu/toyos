//! Loom: the idle loop's reap gate.
//!
//! The gate exists so that a CPU with nothing to run does not take
//! `PROCESS_TABLE` on every trip round the idle loop — the standing aggressor
//! against the crash report's `try_lock`, which is the whole reason a fault
//! report could print a bare address for a symbol that was right there
//! (`specs/issues/panic-path/`). Gating housekeeping on a flag is only sound if
//! the flag cannot lose a raise, and that is what these models are for: a raise
//! concurrent with a claim, from every interleaving loom can build.
//!
//! Two directions, and both matter:
//!
//! * **Nothing enrolled, nothing claimed** — the property the fix is for. A
//!   gate nobody raised answers `false`, so the idle loop takes no lock.
//! * **A raise is never dropped** — the property the fix must not cost. Work
//!   enrolled is work some trip claims, and the claimer sees it.

use kernel_loom::reap_gate::ReapGate;
use loom::sync::atomic::{AtomicUsize, Ordering};
use loom::sync::Arc;

/// An idle CPU asking a gate nobody raised must be told there is nothing to do.
///
/// Single-threaded, and deliberately so: this is the answer on the path that
/// runs continuously on an otherwise empty machine, and it must not depend on
/// what any other CPU is doing.
#[test]
fn an_unraised_gate_claims_nothing() {
    loom::model(|| {
        let gate = ReapGate::new();
        assert!(!gate.take(), "an idle trip claimed work nobody enrolled");
        assert!(!gate.take(), "the second trip claimed it too");
    });
}

/// A raise that races a claim is never lost: either that claim took it, or the
/// flag is still up for the next trip.
///
/// This is the model that would fail if `take` cleared the flag *after* doing
/// the work rather than before, which is the shape the fix deliberately does
/// not have.
#[test]
fn a_raise_is_never_dropped() {
    loom::model(|| {
        let gate = Arc::new(ReapGate::new());

        let raiser = {
            let gate = gate.clone();
            loom::thread::spawn(move || gate.raise())
        };

        let claimed = gate.take();
        raiser.join().unwrap();

        assert!(
            claimed || gate.take(),
            "the raise was dropped: nobody claimed it and the flag is down, so a poisoned \
             thread's waiter would wait for ever",
        );
    });
}

/// A claimer sees the work the raise was about.
///
/// `poison_tid` writes its slot and *then* raises; `publish_exit` stores
/// `finished` and *then* raises. The reaper reads both with the process table
/// held and nothing else ordering it against the raiser, so the gate's own
/// release/acquire pair is the whole edge. Weaken `raise` to `Relaxed` and this
/// is the model that reds.
#[test]
fn a_claim_sees_the_enrolled_work() {
    loom::model(|| {
        let gate = Arc::new(ReapGate::new());
        // Stands for the poison slot, or for the object's `finished` flag.
        let work = Arc::new(AtomicUsize::new(0));

        let raiser = {
            let (gate, work) = (gate.clone(), work.clone());
            loom::thread::spawn(move || {
                work.store(1, Ordering::Relaxed);
                gate.raise();
            })
        };

        if gate.take() {
            assert_eq!(
                work.load(Ordering::Relaxed),
                1,
                "a claimed gate handed the reaper an empty poison slot — the raise did not \
                 carry the work it was raised for",
            );
        }
        raiser.join().unwrap();
    });
}

/// Two idle CPUs, one raise: the work is claimed once.
///
/// A double claim would only cost a second, empty pass over the table, but the
/// gate is a claim and not a hint, and the models say which.
#[test]
fn one_raise_is_claimed_once() {
    loom::model(|| {
        let gate = Arc::new(ReapGate::new());
        gate.raise();

        let other = {
            let gate = gate.clone();
            loom::thread::spawn(move || gate.take())
        };

        let mine = gate.take();
        let theirs = other.join().unwrap();

        assert!(
            !(mine && theirs),
            "both idle CPUs claimed the same reap — the gate is a claim, not a hint",
        );
        assert!(mine || theirs, "neither CPU claimed a raise that had already happened");
    });
}
