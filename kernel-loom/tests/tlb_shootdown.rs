//! Loom: the shootdown acknowledgement's happens-before edges.
//!
//! The property under test is the one the whole stage exists for:
//!
//! > when an initiator is told that cpu 1 has flushed, cpu 1's translations
//! > already reflect the page-table write the initiator made before asking.
//!
//! A TLB is modelled as one cell holding what cpu 1's last page walk found, and
//! a flush is modelled as re-walking — the strongest thing a flush can be asked
//! to mean and the only one expressible here. So `UNMAPPED` after an
//! acknowledged shootdown is "cpu 1 can no longer reach the page the initiator
//! is about to free", and `MAPPED` is the use-after-free this stage removes.
//!
//! **Every spin is bounded and every assertion is conditional on the ack having
//! arrived, so [`ACKED`] is what stops the models passing vacuously.** An
//! unbounded serve loop is the same trap `kernel-loom`'s lock models document:
//! loom explores it as an unbounded branch and never finishes. Safety is what
//! the bounded form checks, and safety is the whole property — liveness in the
//! kernel comes from the IPI being redelivered, which is hardware and not this
//! protocol.

use std::sync::atomic::{AtomicBool, Ordering::SeqCst};

use kernel_loom::shootdown::Shootdown;
use loom::sync::Arc;
use loom::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// The page-table entry every model writes: 1 mapped, 0 unmapped.
const MAPPED: u64 = 1;
const UNMAPPED: u64 = 0;

/// How many times cpu 1 takes the interrupt. Two, because the first may land
/// before the initiator's `issue` is visible and publish a generation that does
/// not answer it — which is a schedule the kernel resolves by the vector staying
/// pending, and which this bound stands in for.
///
/// A `while` loop waiting for the IPI would be truer to the kernel and is not
/// available: loom explores a spin as an unbounded branch and does not finish —
/// measured here, twice, before this became a counted loop.
const SERVES: usize = 2;

/// How many times the initiator looks for its acknowledgement.
const POLLS: usize = 3;

/// Every model here explores interleavings with at most this many preemptions.
///
/// Unbounded, neither model finishes: measured at over seven minutes each with
/// no verdict, which is the same wall this crate's lock models hit. Bounded at
/// two, both run in about ten seconds and both negative controls are still
/// caught — which is the check that matters, because a bound that hides the
/// controls would be a bound that hides the defect too.
const PREEMPTIONS: usize = 2;

fn model(f: impl Fn() + Sync + Send + 'static) {
    let mut builder = loom::model::Builder::new();
    builder.preemption_bound = Some(PREEMPTIONS);
    builder.check(f);
}

/// Set by any execution in which the initiator's wait actually completed.
///
/// Outside `loom::model` on purpose: loom re-runs the closure once per
/// interleaving, so a flag inside it says nothing about the model as a whole.
/// Without this, bounding the spins would make every assertion below skippable
/// and a broken protocol could pass by never being asked.
static ACKED: AtomicBool = AtomicBool::new(false);

struct Machine {
    shootdown: Shootdown,
    /// The page-table entry the initiator unmaps.
    pte: AtomicU64,
    /// What cpu 1's last page walk found. A flush re-walks.
    tlb: AtomicU64,
}

impl Machine {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            shootdown: Shootdown::new(),
            pte: AtomicU64::new(MAPPED),
            tlb: AtomicU64::new(MAPPED),
        })
    }

    /// cpu 1's side: take the interrupt and run vector 0xFE's body.
    fn take_interrupts(&self) {
        for _ in 0..SERVES {
            loom::thread::yield_now();
            self.shootdown
                .serve(1, || self.tlb.store(self.pte.load(Relaxed), Relaxed));
        }
    }

    /// The initiator's side, up to the point where it would free the pages.
    /// `None` when the bounded poll ran out, which is a schedule this model
    /// says nothing about.
    fn unmap_and_wait(&self) -> Option<u64> {
        self.pte.store(UNMAPPED, Relaxed);
        let generation = self.shootdown.issue();
        for _ in 0..POLLS {
            if self.shootdown.served(1, generation) {
                ACKED.store(true, SeqCst);
                return Some(self.tlb.load(Relaxed));
            }
            loom::thread::yield_now();
        }
        None
    }
}

/// An acknowledged shootdown means cpu 1 has re-walked since the unmap.
#[test]
fn an_acknowledged_flush_postdates_the_page_table_write() {
    ACKED.store(false, SeqCst);
    model(|| {
        let m = Machine::new();

        let target = {
            let m = m.clone();
            loom::thread::spawn(move || m.take_interrupts())
        };

        if let Some(reachable) = m.unmap_and_wait() {
            assert_eq!(
                reachable, UNMAPPED,
                "cpu 1 acknowledged the shootdown while still holding a \
                 translation for the page the initiator is about to free",
            );
        }

        target.join().unwrap();
    });
    assert!(
        ACKED.load(SeqCst),
        "no interleaving completed the wait, so the assertion never ran",
    );
}

/// Two initiators, one target: one serve may answer both, which is what makes a
/// single pending IPI bit sufficient.
///
/// The vector's IRR is one bit per CPU, so a second shootdown raised while the
/// handler runs coalesces with the first. That is only sound because a serve
/// publishes the *latest* generation it saw rather than the one that woke it —
/// this model is what says so.
#[test]
fn one_serve_answers_two_concurrent_shootdowns() {
    ACKED.store(false, SeqCst);
    model(|| {
        let m = Machine::new();

        let target = {
            let m = m.clone();
            loom::thread::spawn(move || m.take_interrupts())
        };

        let second = {
            let m = m.clone();
            loom::thread::spawn(move || m.unmap_and_wait())
        };

        if let Some(reachable) = m.unmap_and_wait() {
            assert_eq!(reachable, UNMAPPED, "the first initiator freed a page cpu 1 could still reach");
        }
        if let Some(reachable) = second.join().unwrap() {
            assert_eq!(reachable, UNMAPPED, "the second initiator freed a page cpu 1 could still reach");
        }

        target.join().unwrap();
    });
    assert!(
        ACKED.load(SeqCst),
        "no interleaving completed a wait, so the assertions never ran",
    );
}

/// A CPU that is waiting for an acknowledgement has not stopped giving them.
///
/// **Not an interleaving question, so loom explores nothing here** — the
/// schedule below is written out because it is the one two CPUs took on
/// 2026-08-07, and it is the whole defect. `loom::model` is still what wraps it:
/// this crate compiles `shootdown.rs` against loom's atomics, which panic
/// outside a model, and the property under test is the protocol's shape rather
/// than any edge between its atomics.
///
/// `IF` is clear for the whole of a syscall (`arch::syscall`'s `MSR_FMASK`), so
/// neither CPU below can take the other's IPI. Each one's own wait is the only
/// thing left that can answer the other, and until this test existed the wait
/// asked without answering: both spun until `ACK_TIMEOUT_NS` killed them, as a
/// double kernel panic and as seven wide-phase test failures sharing one line.
#[test]
fn an_initiator_answers_while_it_waits() {
    model(|| {
        let s = Shootdown::new();

        // cpu 0 unmaps and answers for itself; cpu 1 has not issued yet, so the
        // generation cpu 0 publishes is its own.
        let g0 = s.issue();
        s.serve(0, || {});

        // cpu 1 unmaps while cpu 0 is between its own `serve` and its wait.
        let g1 = s.issue();

        // cpu 0's wait is not satisfied — cpu 1 has not flushed — and that turn
        // is nevertheless where cpu 1's answer comes from.
        assert!(!s.wait_turn(0, 1, g0, || {}));
        assert!(
            s.served(0, g1),
            "cpu 0 is waiting for cpu 1 and has stopped answering it, which is \
             a wait neither CPU can leave",
        );

        // And cpu 1's own wait then has an answer to find.
        s.serve(1, || {});
        assert!(s.wait_turn(1, 0, g1, || {}));
        assert!(s.wait_turn(0, 1, g0, || {}));
    });
}
