//! The capability whose whole authority is in the rights on the handle.
//!
//! Three things are reachable no other way — minting a device claim, entering
//! the real-time band, and turning a pid into a process handle — and each is
//! one bit on a handle to this. The kernel makes exactly one at boot, for
//! `/bin/init`, so the set of processes that can ever do any of the three is
//! exactly what init endowed.

use toyos_abi::syscall::{self, DeviceType, SyscallError};

use crate::{AsHandle, Device, OwnedHandle, RawHandle};

pub struct SysCap(pub(crate) OwnedHandle);

impl SysCap {
    /// Mint the claim for a device class.
    ///
    /// `NotFound` is a machine with no such device, which is a fact init logs
    /// and endows nothing for — not a failure.
    pub fn claim(&self, class: DeviceType) -> Result<Device, SyscallError> {
        syscall::device_claim(self.0.fd(), class).map(|h| Device(OwnedHandle(h)))
    }

    /// Enter the real-time band. A device claim was never enough to confer
    /// this; a right is.
    pub fn enter_rt(&self) -> Result<(), SyscallError> {
        syscall::rt_enter(self.0.fd())
    }

    /// Give up ownership, for a handle about to be endowed.
    pub fn into_raw(self) -> RawHandle {
        self.0.into_raw()
    }
}

impl AsHandle for SysCap {
    fn as_handle(&self) -> RawHandle {
        self.0.fd()
    }
}
