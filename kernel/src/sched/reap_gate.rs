//! The one question the idle loop asks before it takes the process table.
//!
//! `scheduler::reap_poisoned` used to take `PROCESS_TABLE` unconditionally, on
//! every trip round the idle loop, whether or not there was anything in it to
//! reap — and on a machine with nothing to run that is every CPU, continuously.
//! The crash report's `process::with_user_symbols` reads the same table through
//! a `try_lock` it must not block on, so the housekeeping was a standing
//! aggressor against the one reader that cannot wait: a fault taken while some
//! CPU sat between the top of the loop and its `pass` lost the faulting
//! function's name to a lock that was merely held. This gate is what makes the
//! common case — nothing to reap — cost no lock at all.
//!
//! Pure, and deliberately free of any `crate::` reference: `kernel-loom/`
//! compiles this file with `feature = "loom"` on, so the models below drive the
//! real flag rather than a transliteration of it.
//!
//! **The whole correctness argument is that a raise is never lost.** The flag
//! is set by every site that creates work for the idle loop and cleared only by
//! the trip that claims it, *before* that trip does the work. A raise that
//! lands after the claim leaves the flag set, so the next trip does the work
//! again; a raise that lands before it is what the claim saw. A spurious claim
//! costs one uncontended lock acquisition and nothing else, which is the
//! direction this is allowed to be wrong in.

#[cfg(not(feature = "loom"))]
use core::sync::atomic::{AtomicBool, Ordering};

#[cfg(feature = "loom")]
use loom::sync::atomic::{AtomicBool, Ordering};

/// The store that enrols work, and what it carries.
///
/// A raise is made *after* the work it is about — the poison slot written, the
/// process's exit stored — so this release is the whole edge between the raiser
/// and the claimer: the reaper reads both with the process table held and
/// nothing else ordering it against the raiser.
///
/// **A cargo feature rather than a comment, because a model that has never
/// failed proves nothing.** `kernel-loom`'s `reap-raise-relaxed` makes it
/// `Relaxed` and `kernel-loom/tests/reap_gate.rs` must red under it, at
/// `a_claim_sees_the_enrolled_work` — a claimed gate then hands the reaper an
/// empty poison slot, which is the class x86's TSO hides from every guest test
/// in this tree. No kernel build can turn the name on: the kernel declares it
/// only so `cfg` checking knows it.
#[cfg(not(feature = "reap-raise-relaxed"))]
const ENROL: Ordering = Ordering::Release;
#[cfg(feature = "reap-raise-relaxed")]
const ENROL: Ordering = Ordering::Relaxed;

/// Whether the idle loop has cleanup waiting for it.
pub struct ReapGate {
    pending: AtomicBool,
}

impl ReapGate {
    /// The gate is a `static` in the kernel, so this must stay `const`. Loom's
    /// atomics have no const constructor, hence the second arm — `sync::Lock`
    /// carries the same pair for the same reason.
    #[cfg(not(feature = "loom"))]
    pub const fn new() -> Self {
        Self { pending: AtomicBool::new(false) }
    }

    #[cfg(feature = "loom")]
    pub fn new() -> Self {
        Self { pending: AtomicBool::new(false) }
    }

    /// Record that there is something to reap.
    ///
    /// Called *after* the work itself is published — the poison slot written,
    /// the process's exit stored — so that the release here carries it: a
    /// claimer that reads this store reads everything the raiser did before it.
    pub fn raise(&self) {
        self.pending.store(true, ENROL);
    }

    /// Claim the pending work, if there is any.
    ///
    /// `true` exactly once per raise-and-claim: the claimer owns the cleanup
    /// and every other CPU round the loop is told there is nothing to do. The
    /// acquire pairs with [`raise`](Self::raise), so a claimer sees the work
    /// that motivated the raise.
    ///
    /// The relaxed load before the swap is the point of the whole file. Every
    /// CPU with nothing to run comes round this question continuously; an
    /// unconditional read-modify-write would have traded a contended lock for a
    /// contended cache line, which is the same bus traffic wearing a smaller
    /// hat — and under TCG, where every guest here is measured, an uncontended
    /// atomic RMW on a hot path is priced unlike hardware besides. A load
    /// leaves the line shared, and a store this misses is seen on the next trip
    /// — the flag stays set until somebody claims it.
    pub fn take(&self) -> bool {
        if !self.pending.load(Ordering::Relaxed) {
            return false;
        }
        self.pending.swap(false, Ordering::Acquire)
    }
}
