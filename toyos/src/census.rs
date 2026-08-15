//! The kernel's live-object count, per kind.
//!
//! **A total hides a leak.** Every leak assertion in the test estate used to be
//! against one machine-wide number, where an object of one kind that is never
//! released is invisible behind ordinary churn in another — and six of the
//! thirteen kinds (`File`, `Device`, `Acceptor`, `Connection`, `IoUring`,
//! `Console`) were exercised by no census assertion at all, which is where
//! three of this branch's defects lived. The kernel has always counted per
//! kind; the readers that answered a total or wrote the breakdown into the
//! kernel log, where no guest test can see one, are retired, and this is what
//! is left.
//!
//! Two readings and a comparison, never one reading against a constant: an
//! object released by another process is dropped from the deferred queue on
//! whichever CPU drains next, so a single sample can be high by whatever has
//! not drained yet. A leak is not a lag — it accumulates.

use core::fmt;

use toyos_abi::syscall::debug_action::CENSUS_KIND;
use toyos_abi::syscall::{self, OBJECT_KINDS};

/// How many objects of every kind are alive right now.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Census([u64; OBJECT_KINDS.len()]);

impl Census {
    pub fn now() -> Self {
        let mut counts = [0u64; OBJECT_KINDS.len()];
        for (i, slot) in counts.iter_mut().enumerate() {
            *slot = syscall::debug_with(CENSUS_KIND, i as u64);
        }
        Self(counts)
    }

    /// Live objects of one kind. The name is an [`OBJECT_KINDS`] entry and a
    /// name that is not one is a caller with a typo, so it panics rather than
    /// answering zero — which would read as "nothing of that kind leaked".
    pub fn kind(&self, name: &str) -> u64 {
        let i = OBJECT_KINDS
            .iter()
            .position(|k| *k == name)
            .unwrap_or_else(|| panic!("no kernel object kind is called {name:?}"));
        self.0[i]
    }

    pub fn total(&self) -> u64 {
        self.0.iter().sum()
    }

    /// Every kind this reading holds more of than `before`, with both counts.
    pub fn grown_since(
        &self,
        before: &Census,
    ) -> impl Iterator<Item = (&'static str, u64, u64)> + '_ {
        let earlier = before.0;
        OBJECT_KINDS
            .iter()
            .enumerate()
            .filter(move |(i, _)| self.0[*i] > earlier[*i])
            .map(move |(i, name)| (*name, earlier[i], self.0[i]))
    }
}

impl fmt::Display for Census {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for (name, live) in OBJECT_KINDS.iter().zip(self.0.iter()).filter(|(_, n)| **n > 0) {
            if !first {
                write!(f, ", ")?;
            }
            write!(f, "{name} {live}")?;
            first = false;
        }
        if first {
            write!(f, "nothing")?;
        }
        Ok(())
    }
}
