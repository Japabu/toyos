//! The only place a process can resolve a service name.
//!
//! A namespace is **immutable once built**: no insert, no remove, no replace.
//! A narrower one is a new object built from an existing one, so a handle to a
//! namespace is a handle to a fixed set — and a child given a subset cannot
//! widen it back.
//!
//! There is no global registry behind this. A name a process was not given
//! resolves to nothing, and there is no second place to ask.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

use toyos_abi::syscall::{MAX_NAMESPACE_ENTRIES, MAX_SERVICE_NAME};

use super::port::Connector;
use super::{KObjectVariant, ObjectCore};

pub struct Namespace {
    pub(super) core: ObjectCore,
    /// Sorted by name, so a lookup is a binary search and a duplicate is
    /// visible at construction.
    entries: Box<[(Box<str>, Arc<Connector>)]>,
}

/// Why a namespace could not be built.
pub enum BuildError {
    /// More than [`MAX_NAMESPACE_ENTRIES`], or a name past
    /// [`MAX_SERVICE_NAME`]. A caller asking for one more has a bug and is
    /// refused by name rather than truncated to fit.
    TooMany,
    /// Two entries for one name. Which one wins is not something a caller
    /// should have to know.
    Duplicate,
}

impl Namespace {
    /// The entries must already carry every name this namespace is to hold —
    /// a base's kept names are resolved by the caller, because only it can say
    /// which of them to keep.
    pub fn build(mut entries: Vec<(Box<str>, Arc<Connector>)>) -> Result<Arc<Self>, BuildError> {
        if entries.len() > MAX_NAMESPACE_ENTRIES {
            return Err(BuildError::TooMany);
        }
        if entries.iter().any(|(name, _)| name.len() > MAX_SERVICE_NAME) {
            return Err(BuildError::TooMany);
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        if entries.windows(2).any(|w| w[0].0 == w[1].0) {
            return Err(BuildError::Duplicate);
        }
        Ok(Arc::new(Self {
            core: Self::new_core(),
            entries: entries.into_boxed_slice(),
        }))
    }

    pub fn lookup(&self, name: &str) -> Option<&Arc<Connector>> {
        let i = self.entries.binary_search_by(|(n, _)| (**n).cmp(name)).ok()?;
        Some(&self.entries[i].1)
    }

}
