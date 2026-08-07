//! The acknowledgement half of a TLB shootdown, with no hardware in it.
//!
//! Split out from `arch::tlb` for one reason: this file is compiled a second
//! time into `kernel-loom/` against loom's atomics, the way `sync.rs` is. x86's
//! TSO gives every load acquire semantics and every store release semantics, so
//! a missing edge in this protocol is invisible on the only architecture ToyOS
//! boots — reading the code would otherwise be the whole gate, and the ticket
//! lock's own acquire edge sat on the wrong atomic through every green suite run
//! until a model checker was pointed at it.
//!
//! The protocol is one counter and one publication per CPU:
//!
//! - an initiator writes page tables, then [`Shootdown::issue`] names a
//!   generation, then it waits until every other CPU has [`Shootdown::served`]
//!   that generation;
//! - a target reads what is owed, flushes, and publishes what it read.
//!
//! **The read is before the flush, and that ordering is the protocol.** A target
//! that read the counter after flushing could publish a generation whose
//! page-table write its own flush had not yet seen, and the initiator would free
//! memory the target still had a translation for — the exact defect this file
//! exists to remove, wearing an acknowledgement.

#[cfg(not(feature = "loom"))]
use core::sync::atomic::{AtomicU64, Ordering};

#[cfg(feature = "loom")]
use loom::sync::atomic::{AtomicU64, Ordering};

/// Matches `sched::MAX_CPUS`. Kept as its own constant because this file has no
/// `crate::` references at all — that is what lets loom compile it.
pub const MAX_CPUS: usize = 8;

/// Which shootdown a flush answers for.
///
/// Monotonic and machine-wide, so "has cpu N flushed since I wrote the page
/// table" is one comparison rather than a handshake per CPU. A target that
/// serves once while two initiators are waiting satisfies both, which is what
/// keeps the IPI vector's single pending bit sufficient.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Generation(u64);

pub struct Shootdown {
    requested: AtomicU64,
    flushed: [AtomicU64; MAX_CPUS],
}

impl Shootdown {
    /// The kernel's instance is a `static`, so this must stay `const`. Loom's
    /// atomics have no const constructor, hence the second arm.
    #[cfg(not(feature = "loom"))]
    pub const fn new() -> Self {
        Self {
            requested: AtomicU64::new(0),
            flushed: [const { AtomicU64::new(0) }; MAX_CPUS],
        }
    }

    #[cfg(feature = "loom")]
    pub fn new() -> Self {
        Self {
            requested: AtomicU64::new(0),
            flushed: core::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    /// Name the generation every other CPU now owes a flush for.
    ///
    /// `AcqRel` and not `Release`: the release half publishes the page-table
    /// writes the caller made before it, and the acquire half is what stops a
    /// later load — the wait's first look at `flushed` — being hoisted above
    /// this point and reading an acknowledgement from before the write.
    pub fn issue(&self) -> Generation {
        Generation(self.requested.fetch_add(1, Ordering::AcqRel) + 1)
    }

    /// A target's whole side of the protocol.
    ///
    /// The `Acquire` load is what makes `flush` see the initiator's page-table
    /// write: it synchronizes with [`issue`](Self::issue)'s release, so
    /// everything the initiator did before issuing is visible here before the
    /// flush walks anything. Publishing a value read *after* the flush would
    /// claim more than the flush did.
    pub fn serve(&self, cpu: usize, flush: impl FnOnce()) {
        let owed = self.requested.load(Ordering::Acquire);
        flush();
        self.flushed[cpu].store(owed, Ordering::Release);
    }

    /// Does `cpu` owe anyone a flush?
    ///
    /// A hint, and `Relaxed` because it is one: a stale `false` costs a spinning
    /// CPU nothing, since the interrupt it is not taking is still pending and
    /// the next look answers. [`serve`](Self::serve) is where the ordering is,
    /// and a caller of this always calls that.
    pub fn owes(&self, cpu: usize) -> bool {
        self.requested.load(Ordering::Relaxed) > self.flushed[cpu].load(Ordering::Relaxed)
    }

    /// Has `cpu` flushed since `generation` was issued?
    ///
    /// `Acquire` so that a caller which sees the answer also sees everything the
    /// target did before publishing it. Nothing in the kernel reads through that
    /// edge today — the initiator only frees memory — but a `Relaxed` load here
    /// would be a load-bearing omission the moment one does.
    pub fn served(&self, cpu: usize, generation: Generation) -> bool {
        self.flushed[cpu].load(Ordering::Acquire) >= generation.0
    }
}
