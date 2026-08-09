//! The one object whose whole authority is in the rights on the handle.
//!
//! Three things in this system are not reachable by holding some other object:
//! minting a device claim, entering the RT band, and turning a pid into a
//! process handle. Each is one bit on a handle to this, and the kernel creates
//! exactly one full-rights `SysCap` at boot for `/bin/init`. Nothing else can
//! construct one, so the set of processes that can ever do any of the three is
//! exactly what init endowed.

use alloc::sync::Arc;

use super::{KObjectVariant, ObjectCore};

pub struct SysCap {
    /// Visible to `object`, because that is where [`kobject!`] generates every
    /// type's `core()`.
    ///
    /// [`kobject!`]: super
    pub(super) core: ObjectCore,
}

impl SysCap {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { core: Self::new_core() })
    }
}
