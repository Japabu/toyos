//! What every i8042 interrupt turned out to be, in one word.
//!
//! Two numbers are wanted from the pin: how often it has asserted, and how many
//! of those assertions the ISR found nothing behind. Every verdict the driver
//! prints turns on the difference — "nothing decoded" is a claim about *bytes*,
//! and an edge whose byte the polling init already took is not one.
//!
//! **They were two counters and the difference was read torn.** The ISR added
//! to `IRQS` on entry and to `EMPTY_IRQS` after the drain came back empty, with
//! the whole port-drain loop between them, and the report computed
//! `carried = IRQS - EMPTY_IRQS`. A reader landing inside that window attributed
//! an empty interrupt to `carried` and printed a verdict about bytes that never
//! arrived. Widening the window was not the defect and narrowing it was not the
//! fix: *any* window is one, because the report is a statement about a completed
//! observation and the ISR had not finished making it.
//!
//! So the pair is one `u64` and the ISR writes it once, after the burst, when
//! what it says is settled: the low half counts the interrupts that put a byte
//! in the ring and the high half those that found none. A reader takes one load
//! and gets a pair that agreed at one instant, which is the only kind of pair
//! there is now — [`Counts`] cannot be built any other way. There is no
//! subtraction left to be wrong, and `carried` never runs ahead of the bytes it
//! is a count of, at any instant rather than eventually.
//!
//! The release on the write and the acquire on the read are the other half of
//! that: a reader that sees an interrupt counted sees everything the ISR did for
//! it — the bytes pushed into the ring and the arrival stamps — rather than a
//! count whose evidence has not landed yet. On x86 both are plain instructions.
//!
//! **Sole writer.** Delivery is pinned to one CPU behind an interrupt gate, so
//! [`Tally::record`] never runs against itself; the saturation guard reads
//! before it adds and that is what makes the read exact. Every other CPU only
//! loads.
//!
//! Pure, and deliberately free of any `crate::` reference: `kernel-loom/`
//! compiles this file with `feature = "loom"` on, so
//! `kernel-loom/tests/i8042_tally.rs` drives the real word rather than a
//! transliteration of it — and x86's TSO would hide the ordering above from
//! every guest test there is.

#[cfg(not(feature = "loom"))]
use core::sync::atomic::{AtomicU64, Ordering};

#[cfg(feature = "loom")]
use loom::sync::atomic::{AtomicU64, Ordering};

/// What one interrupt turned out to be. Decided by the ISR after its burst, so
/// it is never a prediction.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Carried {
    /// The burst read at least one byte, and every one of them reached the ring
    /// before this was recorded.
    Bytes,
    /// OBF was clear on the first sample: the edge carried nothing. The
    /// driver's own polling init is what produces these — it takes the answers
    /// to its commands with the pin already armed, so the interrupt those bytes
    /// raised arrives to an empty output buffer.
    Nothing,
}

/// One consistent reading of the pair.
///
/// **Only [`Tally::read`] can build one**, and it is a single load — so the two
/// numbers here always describe the same instant, and no caller can assemble a
/// pair that never existed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Counts {
    /// Interrupts that delivered at least one byte into the ring.
    ///
    /// The number every "did anything arrive to decode" question wants. It
    /// moves only after the bytes are visible, so a reader that sees it move
    /// can find them.
    pub carried: u32,
    /// Interrupts the ISR found nothing behind.
    pub empty: u32,
}

impl Counts {
    /// Times the pin has actually asserted — which is the sum, because an
    /// interrupt is one or the other and the ISR classifies every one of them.
    ///
    /// Saturating for [`Tally::record`]'s reason: the halves stop rather than
    /// wrap, and a total that wrapped past its parts would be the one number
    /// here that could not be checked against anything.
    pub fn irqs(self) -> u32 {
        self.carried.saturating_add(self.empty)
    }
}

/// The low half's unit. The two are disjoint by construction: an interrupt adds
/// to exactly one of them, so nothing is ever subtracted to answer a question.
const CARRIED_ONE: u64 = 1;
const EMPTY_ONE: u64 = 1 << 32;

pub struct Tally {
    packed: AtomicU64,
}

impl Tally {
    /// The tally is a `static` in the kernel, so this must stay `const`. Loom's
    /// atomics have no const constructor, hence the second arm — `sync::Lock`
    /// and `sched::reap_gate` carry the same pair for the same reason.
    #[cfg(not(feature = "loom"))]
    pub const fn new() -> Self {
        Self { packed: AtomicU64::new(0) }
    }

    #[cfg(feature = "loom")]
    pub fn new() -> Self {
        Self { packed: AtomicU64::new(0) }
    }

    /// Account for one interrupt.
    ///
    /// **Called once, from the ISR, after the burst** — never on the way in.
    /// That is the whole shape: the classification is what the burst found, and
    /// a count published before the burst ran is a claim about an observation
    /// that has not been made. The release carries the burst's own writes with
    /// it.
    ///
    /// Saturating rather than wrapping, and the guard is what makes the packing
    /// safe: a `fetch_add` on a full low half carries into the high one, so an
    /// overflow here would not merely wrap one diagnostic — it would corrupt the
    /// other. Both halves stop together at `u32::MAX`, because the word is one
    /// number. The load is exact because the ISR is the sole writer.
    pub fn record(&self, carried: Carried) {
        let counts = Self::split(self.packed.load(Ordering::Relaxed));
        if counts.carried == u32::MAX || counts.empty == u32::MAX {
            return;
        }
        let one = match carried {
            Carried::Bytes => CARRIED_ONE,
            Carried::Nothing => EMPTY_ONE,
        };
        self.packed.fetch_add(one, Ordering::Release);
    }

    /// The pair, as it stood at one instant.
    ///
    /// The acquire pairs with [`record`](Self::record): a reader that sees an
    /// interrupt counted sees the bytes it delivered. Every reader of these
    /// numbers is about to say something about those bytes, so that edge is the
    /// difference between a report and a guess.
    pub fn read(&self) -> Counts {
        Self::split(self.packed.load(Ordering::Acquire))
    }

    fn split(packed: u64) -> Counts {
        Counts { carried: packed as u32, empty: (packed >> 32) as u32 }
    }
}
