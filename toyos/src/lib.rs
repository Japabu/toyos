//! ToyOS userland SDK.
//!
//! Typed handles, IPC framing, service discovery, shared memory, and
//! ergonomic wrappers over the kernel ABI defined in `toyos-abi`.

#![no_std]

pub mod audio;
pub mod device;
pub mod gpu;
pub mod poller;
pub mod ipc;
pub mod net;
pub mod pipe;
pub mod services;
pub mod shm;
pub mod system;

pub use ipc::Connection;
pub use device::{Keyboard, Mouse, FramebufferDev, Nic, AudioDev};

pub use toyos_abi::Fd;

// ---------------------------------------------------------------------------
// AsHandle — unified trait for typed handles
// ---------------------------------------------------------------------------

/// Trait for types that wrap a kernel handle (fd).
///
/// Used by [`ring::Ring::poll_add`] and other APIs that accept any handle type.
pub trait AsHandle {
    fn as_handle(&self) -> Fd;
}

// ---------------------------------------------------------------------------
// Handle — internal RAII base
// ---------------------------------------------------------------------------

/// Internal base handle. Non-Copy. Drop calls close.
///
/// Not public — consumers use the typed wrappers below.
pub(crate) struct Handle(pub(crate) Fd);

impl Handle {
    pub(crate) fn fd(&self) -> Fd { self.0 }

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

impl Drop for Handle {
    fn drop(&mut self) {
        toyos_abi::syscall::close(self.0);
    }
}

// ---------------------------------------------------------------------------
// Typed handles
// ---------------------------------------------------------------------------

/// A service listener. Created by [`services::listen`].
pub struct Listener(pub(crate) Handle);

impl Listener {
    pub fn fd(&self) -> Fd { self.0.fd() }
}

impl AsHandle for Listener {
    fn as_handle(&self) -> Fd { self.0.fd() }
}

/// A claimed hardware device. Created by [`device::open_keyboard`] etc.
pub struct Device(pub(crate) Handle);

impl Device {
    pub fn fd(&self) -> Fd { self.0.fd() }

    pub fn read(&self, buf: &mut [u8]) -> Result<usize, toyos_abi::syscall::SyscallError> {
        self.0.read(buf)
    }
}

impl AsHandle for Device {
    fn as_handle(&self) -> Fd { self.0.fd() }
}

/// A kernel pipe endpoint. Created by [`pipe::open_by_id`].
pub struct Pipe(pub(crate) Handle);

impl Pipe {
    pub fn fd(&self) -> Fd { self.0.fd() }

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

    /// Consume the Pipe, returning the raw fd without closing it.
    pub fn into_fd(self) -> Fd {
        let fd = self.0.fd();
        core::mem::forget(self);
        fd
    }
}

impl AsHandle for Pipe {
    fn as_handle(&self) -> Fd { self.0.fd() }
}
