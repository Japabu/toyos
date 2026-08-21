//! A process, as a thing you hold rather than a number you know.
//!
//! A pid names a process the way a street name names a house: everyone can say
//! it and saying it is not a key. `SYS_SPAWN` answers with one of these, the
//! launcher sends one back, and there is no other way to get one — so what may
//! wait for a process, kill it or read its accounting is exactly what was given
//! a handle to it.

use toyos_abi::handle::Rights;
use toyos_abi::syscall::{self, ProcessStats, SyscallError};

use crate::endow::FromHandle;
use crate::{AsHandle, OwnedHandle, RawHandle};

pub struct Process(pub(crate) OwnedHandle);

impl Process {
    /// Block until it exits, and take the code.
    ///
    /// **Repeatable and never missed.** The code is on the object, so waiting a
    /// second time answers the same thing and waiting long after the process is
    /// gone still answers.
    pub fn wait(&self) -> Result<i32, SyscallError> {
        syscall::process_wait(self.0.raw())
    }

    /// The exit code if it has already exited, `Err(WouldBlock)` if not.
    pub fn try_wait(&self) -> Result<i32, SyscallError> {
        syscall::process_wait_nonblock(self.0.raw())
    }

    /// Kill it. `Ok` for one already dead: the caller asked for it to be gone.
    pub fn kill(&self) -> Result<(), SyscallError> {
        syscall::process_kill(self.0.raw())
    }

    pub fn stats(&self) -> Result<ProcessStats, SyscallError> {
        let mut stats = ProcessStats::default();
        syscall::process_stats(self.0.raw(), &mut stats)?;
        Ok(stats)
    }

    /// A second handle carrying **less** — how a supervisor hands on the right
    /// to wait without the right to kill.
    pub fn narrowed(&self, rights: Rights) -> Result<Self, SyscallError> {
        syscall::dup_narrowed(self.0.raw(), rights).map(|h| Self(OwnedHandle(h)))
    }

    /// Give up ownership, for a handle about to be endowed or sent.
    pub fn into_raw(self) -> RawHandle {
        self.0.into_raw()
    }

    /// # Safety
    /// `raw` must be a live process handle this process owns and nothing else
    /// answers for.
    pub unsafe fn from_raw(raw: RawHandle) -> Self {
        Self(OwnedHandle(raw))
    }
}

impl AsHandle for Process {
    fn as_handle(&self) -> RawHandle {
        self.0.raw()
    }
}

impl FromHandle for Process {
    unsafe fn from_handle(raw: RawHandle) -> Self {
        Self(OwnedHandle(raw))
    }
}
