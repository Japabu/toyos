//! ToyOS userland SDK.
//!
//! Typed handles, IPC framing, ports and namespaces, shared memory, and
//! ergonomic wrappers over the kernel ABI defined in `toyos-abi`.

#![no_std]

pub mod audio;
pub mod device;
pub mod endow;
pub mod gpu;
pub mod poller;
pub mod ipc;
pub mod namespace;
pub mod net;
pub mod pipe;
pub mod port;
pub mod surface;
pub mod shm;
pub mod syscap;
pub mod system;

pub use ipc::Connection;
pub use device::{Keyboard, Mouse, FramebufferDev, Nic, VirtioSoundDev, HdaDev};

pub use toyos_abi::RawHandle;

/// Trait for types that wrap a kernel handle.
///
/// Used by [`poller`] and other APIs that accept any handle type.
pub trait AsHandle {
    fn as_handle(&self) -> RawHandle;
}

/// One owned handle, closed when it drops.
///
/// `!Copy` and `!Clone`, so a handle cannot be closed twice and cannot be
/// forgotten by accident — [`OwnedHandle::into_raw`] is the single spelling for
/// giving up ownership, and the single thing to grep for when asking who does.
///
/// Not public — consumers use the typed wrappers below.
pub(crate) struct OwnedHandle(pub(crate) RawHandle);

impl OwnedHandle {
    pub(crate) fn fd(&self) -> RawHandle { self.0 }

    /// Give up ownership: the handle stays open and this stops answering for
    /// it.
    pub(crate) fn into_raw(self) -> RawHandle {
        let raw = self.0;
        core::mem::forget(self);
        raw
    }

    pub(crate) fn read(&self, buf: &mut [u8]) -> Result<usize, toyos_abi::syscall::SyscallError> {
        toyos_abi::syscall::read(self.0, buf)
    }

    pub(crate) fn write(&self, buf: &[u8]) -> Result<usize, toyos_abi::syscall::SyscallError> {
        toyos_abi::syscall::write(self.0, buf)
    }

    pub(crate) fn read_nonblock(&self, buf: &mut [u8]) -> Result<usize, toyos_abi::syscall::SyscallError> {
        toyos_abi::syscall::read_nonblock(self.0, buf)
    }

    pub(crate) fn write_nonblock(&self, buf: &[u8]) -> Result<usize, toyos_abi::syscall::SyscallError> {
        toyos_abi::syscall::write_nonblock(self.0, buf)
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        toyos_abi::syscall::close(self.0);
    }
}

/// A claimed hardware device, out of this process's endowment table.
///
/// There is no `open`: `/bin/init` mints every claim from the machine's one
/// system capability and endows it, so which process drives a device is a fact
/// the image was built with. See [`endow::device`].
pub struct Device(pub(crate) OwnedHandle);

impl Device {
    pub fn fd(&self) -> RawHandle { self.0.fd() }

    /// Give up ownership, for a claim about to be endowed. A claim carries no
    /// `DUP` right, so this is the only way one changes hands.
    pub fn into_raw(self) -> RawHandle { self.0.into_raw() }

    pub fn read(&self, buf: &mut [u8]) -> Result<usize, toyos_abi::syscall::SyscallError> {
        self.0.read(buf)
    }
}

impl AsHandle for Device {
    fn as_handle(&self) -> RawHandle { self.0.fd() }
}

/// A kernel pipe endpoint. Created by [`pipe::open_by_id`].
pub struct Pipe(pub(crate) OwnedHandle);

impl Pipe {
    pub fn fd(&self) -> RawHandle { self.0.fd() }

    pub fn read(&self, buf: &mut [u8]) -> Result<usize, toyos_abi::syscall::SyscallError> {
        self.0.read(buf)
    }

    pub fn write(&self, buf: &[u8]) -> Result<usize, toyos_abi::syscall::SyscallError> {
        self.0.write(buf)
    }

    pub fn read_nonblock(&self, buf: &mut [u8]) -> Result<usize, toyos_abi::syscall::SyscallError> {
        self.0.read_nonblock(buf)
    }

    pub fn write_nonblock(&self, buf: &[u8]) -> Result<usize, toyos_abi::syscall::SyscallError> {
        self.0.write_nonblock(buf)
    }

    pub fn pipe_map(&self) -> Result<*mut u8, toyos_abi::syscall::SyscallError> {
        toyos_abi::syscall::pipe_map(self.fd())
    }

    pub fn pipe_id(&self) -> Result<u64, toyos_abi::syscall::SyscallError> {
        toyos_abi::syscall::pipe_id(self.fd())
    }

    /// Consume the `Pipe`, giving up the handle without closing it.
    pub fn into_fd(self) -> RawHandle {
        self.0.into_raw()
    }
}

impl AsHandle for Pipe {
    fn as_handle(&self) -> RawHandle { self.0.fd() }
}
