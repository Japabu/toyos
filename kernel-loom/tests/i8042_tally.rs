//! Loom: the i8042's interrupt tally.
//!
//! The driver prints `N interrupts and M bytes, nothing decoded` — a verdict
//! about *bytes* — and it decides whether to print it from how many interrupts
//! carried one. That was two counters the ISR wrote at either end of its
//! port-drain burst, subtracted by a reader on another CPU, and a reader landing
//! between the two writes blamed an interrupt that had carried nothing. It fired
//! about one full suite in three under load
//! (`specs/issues/kernel/an-i8042-interrupt-arrives-with-no-byte-during-init.md`).
//!
//! **A rate is why this is a model and not a test.** The window is a handful of
//! instructions on one CPU; no guest boot can be made to land in it on demand,
//! and a green suite says nothing about whether it can still be landed in. What
//! the property actually is — *at no instant may a reader see an empty interrupt
//! counted as one that carried* — is a claim about every interleaving, which is
//! what loom enumerates.
//!
//! Three directions, and the third is the one that makes the first two mean
//! anything:
//!
//! * **The property.** An interrupt that delivered nothing is never counted as
//!   one that did, from any interleaving.
//! * **The cost it must not have.** An interrupt that *did* deliver is counted,
//!   and a reader that sees the count can see the bytes.
//! * **The teeth.** The same reader, against the shape this replaced, must
//!   actually observe the bad state. A model that has never seen a defect is a
//!   model nobody knows is exploring anything.
//!
//! **Both of the first two red when the two counters are put back**, measured by
//! putting them back: `Counts { carried: 1, empty: 0 }` for an interrupt that
//! carried nothing, and a count of an arrived byte with the byte not yet
//! published. That run is the evidence this file works and the evidence the fix
//! does; the guest suite can supply neither, because a rate is not falsified by
//! a green boot.

use std::sync::atomic::{AtomicBool, Ordering as StdOrdering};

use kernel_loom::i8042_tally::{Carried, Counts, Tally};
use loom::sync::atomic::{AtomicU32, Ordering};
use loom::sync::Arc;

/// An interrupt the ISR found nothing behind is never counted as one that
/// carried a byte — at the settled end, and at every instant on the way there.
///
/// The reader is `service`'s `report_health` on any other CPU: it runs at the
/// top of every scheduler pass, so it is genuinely concurrent with the ISR and
/// is not synchronized against it by anything.
#[test]
fn an_empty_interrupt_is_never_counted_as_one_that_carried() {
    loom::model(|| {
        let tally = Arc::new(Tally::new());

        let isr = {
            let tally = tally.clone();
            loom::thread::spawn(move || tally.record(Carried::Nothing))
        };

        // The verdict's own question: "did anything arrive to decode?"
        let mid = tally.read();
        assert_eq!(
            mid.carried, 0,
            "a reader saw {mid:?} while the only interrupt on the machine carried nothing — \
             `nothing decoded` would have been printed about a byte that never arrived",
        );
        // And the total it prints alongside can never exceed what happened.
        assert!(mid.irqs() <= 1, "a reader saw {mid:?}, which is more interrupts than were taken");

        isr.join().unwrap();
        assert_eq!(
            tally.read(),
            Counts { carried: 0, empty: 1 },
            "the settled pair does not account for the one interrupt that was taken",
        );
    });
}

/// The property the fix must not cost: an interrupt that delivered bytes *is*
/// counted, and a reader that sees the count can see what it delivered.
///
/// The store stands for the byte ring's `HEAD`, which the ISR publishes before
/// it records the interrupt. Every reader of `carried` is about to say something
/// about those bytes — `report_health` checks `has_bytes()` the instant after —
/// so a count visible ahead of its own evidence is a report on a ring that looks
/// empty. **This is the direction that reds on the old shape**: with the two
/// counters back in `tally.rs` it failed here too, `published` still 0 under a
/// count that already said a byte had arrived.
///
/// **It is not a gate on the release/acquire pair, and saying so is the point.**
/// Measured: with `record`'s release and `read`'s acquire both weakened to
/// `Relaxed`, all three models still passed. Loom 0.7 does not distinguish the
/// weakening — which is why this crate's other negative control removes `SeqCst`
/// *fences* rather than weakening an ordering. The pair rests on the argument in
/// `tally.rs`, and on x86 it is the same instruction either way, so no guest can
/// ask the question either.
#[test]
fn a_counted_interrupt_carries_its_bytes_with_it() {
    loom::model(|| {
        let tally = Arc::new(Tally::new());
        let published = Arc::new(AtomicU32::new(0));

        let isr = {
            let (tally, published) = (tally.clone(), published.clone());
            loom::thread::spawn(move || {
                // Everything the interrupt did, before it says it happened.
                published.store(1, Ordering::Relaxed);
                tally.record(Carried::Bytes);
            })
        };

        let seen = tally.read();
        if seen.carried > 0 {
            assert_eq!(
                published.load(Ordering::Relaxed),
                1,
                "a reader counted an interrupt as having delivered a byte and could not see the \
                 byte: the report would say `0 bytes` about one that had arrived",
            );
        }

        isr.join().unwrap();
        assert_eq!(
            tally.read(),
            Counts { carried: 1, empty: 0 },
            "an interrupt that delivered bytes was not counted as one",
        );
    });
}

/// The shape this replaced, kept as the model's own negative control.
///
/// **Deliberately a transliteration**, and the one place in this crate where
/// that is right: it is not the kernel's code any more, and what it has to
/// reproduce is the *arithmetic* — a total written on the way in, an empty count
/// written on the way out, and a reader subtracting one from the other. The
/// point of the model below is that the arithmetic is unsound however faithfully
/// it is copied, which is why narrowing the drain between the two writes was
/// never a fix.
struct TornTally {
    irqs: AtomicU32,
    empty: AtomicU32,
}

impl TornTally {
    fn new() -> Self {
        Self { irqs: AtomicU32::new(0), empty: AtomicU32::new(0) }
    }

    /// `handler()` as it was: `IRQS` on entry, the port-drain burst, then
    /// `EMPTY_IRQS` because the burst read nothing. The burst itself is not
    /// modelled — it is what made the window wide, and the window's *width* is
    /// what this file exists to say is beside the point.
    fn record_empty(&self) {
        self.irqs.fetch_add(1, Ordering::Relaxed);
        self.empty.fetch_add(1, Ordering::Relaxed);
    }

    /// `report_health`'s own line, verbatim.
    fn carried(&self) -> u32 {
        self.irqs.load(Ordering::Relaxed).saturating_sub(self.empty.load(Ordering::Relaxed))
    }
}

/// The teeth: the reader above, against the two counters, really does see the
/// state that printed the false verdict.
///
/// Asserted by collecting rather than by failing, so this stays a passing test
/// that proves a failure is reachable — the flag is an ordinary `std` atomic and
/// therefore outlives loom's executions, and the assertion after `loom::model`
/// is the whole verdict. If a future loom, or a future edit to the model's
/// shape, stops scheduling a reader inside the ISR's window, **this reds** and
/// says so: at that point the two models above are passing because nothing is
/// being explored, which is the one failure a gate must not have.
#[test]
fn the_split_counters_this_replaced_are_read_torn() {
    static TORN: AtomicBool = AtomicBool::new(false);

    loom::model(|| {
        let torn = Arc::new(TornTally::new());

        let isr = {
            let torn = torn.clone();
            loom::thread::spawn(move || torn.record_empty())
        };

        if torn.carried() > 0 {
            TORN.store(true, StdOrdering::Relaxed);
        }

        isr.join().unwrap();
        // Settled, the subtraction was always right. That is exactly why the
        // defect survived review: every interleaving ends in the correct answer
        // and only the instants in between are wrong.
        assert_eq!(torn.carried(), 0, "the split counters disagree once the ISR has finished");
    });

    assert!(
        TORN.load(StdOrdering::Relaxed),
        "no interleaving read the two counters torn, so this model cannot tell the shape that \
         printed the false verdict from the one that replaced it — the two models above are \
         passing on an exploration that is not happening",
    );
}
