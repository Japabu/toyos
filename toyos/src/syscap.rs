//! The capability whose whole authority is in the rights on the handle.
//!
//! Three things are reachable no other way — minting a device claim, entering
//! the real-time band, and turning a pid into a process handle — and each is
//! one bit on a handle to this. The kernel makes exactly one at boot, for
//! `/bin/init`, so the set of processes that can ever do any of the three is
//! exactly what init endowed.

use toyos_abi::handle::Rights;
use toyos_abi::syscall::{self, DeviceType, SyscallError};

use crate::endow::FromHandle;
use crate::{AsHandle, OwnedHandle, RawHandle};

pub struct SysCap(pub(crate) OwnedHandle);

impl SysCap {
    /// Mint the claim for a device class, as whichever typed wrapper the caller
    /// drives it through.
    ///
    /// `NotFound` is a machine with no such device, which is a fact init logs
    /// and endows nothing for — not a failure. `AlreadyExists` is another
    /// process holding the class, which is a different fact and stays loud.
    pub fn claim<T: FromHandle>(&self, class: DeviceType) -> Result<T, SyscallError> {
        let raw = syscall::device_claim(self.0.fd(), class)?;
        // SAFETY: the kernel installed this handle in this process's table for
        // this call and no other, so nothing else answers for it.
        Ok(unsafe { T::from_handle(raw) })
    }

    /// A second handle to this capability, carrying the same rights.
    ///
    /// Only usable by a holder whose own cap carries [`Rights::DUP`], which in
    /// the whole tree is the test estate: its binaries mint their own claims,
    /// and one boot runs several that each need the keyboard.
    pub fn duplicate(&self) -> Result<Self, SyscallError> {
        syscall::dup(self.0.fd()).map(|h| Self(OwnedHandle(h)))
    }

    /// Enter the real-time band. A device claim was never enough to confer
    /// this; a right is.
    pub fn enter_rt(&self) -> Result<(), SyscallError> {
        syscall::rt_enter(self.0.fd())
    }

    /// A second handle to this capability carrying **less**.
    ///
    /// How init gives a program the RT band and nothing else: rights only
    /// shrink, so the dup can never mint a claim or open a process however the
    /// holder asks.
    pub fn narrowed(&self, rights: Rights) -> Result<Self, SyscallError> {
        syscall::dup_narrowed(self.0.fd(), rights).map(|h| Self(OwnedHandle(h)))
    }

    /// A `Process` handle for a pid.
    ///
    /// The one place a pid becomes authority over anything, and only a cap
    /// carrying [`Rights::MANAGE`] reaches it — which in the whole system is
    /// `/bin/init`'s.
    pub fn open_process(&self, pid: toyos_abi::Pid) -> Result<crate::process::Process, SyscallError> {
        let raw = syscall::process_open(self.0.fd(), pid)?;
        // SAFETY: the kernel installed this handle in this process's table for
        // this call and no other.
        Ok(unsafe { crate::process::Process::from_raw(raw) })
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
