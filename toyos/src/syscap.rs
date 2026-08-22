//! The capability whose whole authority is in the rights on the handle.
//!
//! Five things are reachable no other way — minting a device claim, entering
//! the real-time band, turning a pid into a process handle, listing every
//! process in the machine, and powering the machine off — and each is one bit
//! on a handle to this. The kernel makes exactly one at boot, for `/bin/init`,
//! so the set of processes that can ever do any of the five is exactly what
//! init endowed.

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
        let raw = syscall::device_claim(self.0.raw(), class)?;
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
        syscall::dup(self.0.raw()).map(|h| Self(OwnedHandle(h)))
    }

    /// Enter the real-time band. A device claim was never enough to confer
    /// this; a right is.
    pub fn enter_rt(&self) -> Result<(), SyscallError> {
        syscall::rt_enter(self.0.raw())
    }

    /// Power the machine off.
    ///
    /// **Returns only when refused**, because a shutdown that happened has no
    /// caller left to answer. `PermissionDenied` is a capability that does not
    /// carry [`Rights::POWER`] — which is every capability in the machine but
    /// the ones a `system.toml` row named `power` in.
    pub fn shutdown(&self) -> SyscallError {
        syscall::shutdown(self.0.raw())
    }

    /// The machine's header, then one entry per live thread for as much of
    /// `buf` as is left. Answers the bytes written, and `0` if it was refused.
    ///
    /// Needs [`Rights::ROSTER`], which is what makes the entries — a pid, a
    /// size, a CPU time and a **name** for every process in the machine — the
    /// business of `/bin/ps` and not of every program that can make a syscall.
    /// [`crate::system::sysinfo`] is the header on its own and needs nothing.
    ///
    /// A `buf` too small for an entry asks for the header, which this
    /// capability is not needed for and is not consulted about.
    pub fn roster(&self, buf: &mut [u8]) -> usize {
        syscall::sysinfo(self.0.raw(), buf)
    }

    /// A second handle to this capability carrying **less**.
    ///
    /// How init gives a program the RT band and nothing else: rights only
    /// shrink, so the dup can never mint a claim or open a process however the
    /// holder asks.
    pub fn narrowed(&self, rights: Rights) -> Result<Self, SyscallError> {
        syscall::dup_narrowed(self.0.raw(), rights).map(|h| Self(OwnedHandle(h)))
    }

    /// A `Process` handle for a pid.
    ///
    /// The one place a pid becomes authority over anything, and only a cap
    /// carrying [`Rights::MANAGE`] reaches it — which in the whole system is
    /// `/bin/init`'s.
    pub fn open_process(&self, pid: toyos_abi::Pid) -> Result<crate::process::Process, SyscallError> {
        let raw = syscall::process_open(self.0.raw(), pid)?;
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
        self.0.raw()
    }
}
