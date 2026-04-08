//! ToyOS userland SDK.
//!
//! Typed handles, IPC framing, service discovery, shared memory, and
//! ergonomic wrappers over the kernel ABI defined in `toyos-abi`.

#![no_std]

pub mod audio;
pub mod device;
pub mod gpu;
pub mod ring;
pub mod ipc;
pub mod net;
pub mod pipe;
pub mod raw_net;
pub mod services;
pub mod shm;
pub mod system;

use toyos_abi::Fd;

// ---------------------------------------------------------------------------
// Handle — internal RAII base
// ---------------------------------------------------------------------------

/// Internal base handle. Non-Copy. Drop calls close.
///
/// Not public — consumers use the typed wrappers below.
pub(crate) struct Handle(pub(crate) Fd);

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
    pub fn fd(&self) -> Fd { self.0.0 }
}

/// An IPC connection. Created by [`services::accept`] or [`services::connect`].
pub struct Connection(pub(crate) Handle);

impl Connection {
    pub fn fd(&self) -> Fd { self.0.0 }

    pub fn send<T: Copy>(&self, msg_type: u32, payload: &T) -> Result<(), ipc::IpcError> {
        ipc::send(self.fd(), msg_type, payload)
    }

    pub fn signal(&self, msg_type: u32) -> Result<(), ipc::IpcError> {
        ipc::signal(self.fd(), msg_type)
    }

    pub fn send_bytes(&self, msg_type: u32, data: &[u8]) -> Result<(), ipc::IpcError> {
        ipc::send_bytes(self.fd(), msg_type, data)
    }

    pub fn recv_header(&self) -> Result<ipc::IpcHeader, ipc::IpcError> {
        ipc::recv_header(self.fd())
    }

    pub fn recv_payload<T: Copy>(&self, header: &ipc::IpcHeader) -> Result<T, ipc::IpcError> {
        ipc::recv_payload(self.fd(), header)
    }

    pub fn recv<T: Copy>(&self) -> Result<(u32, T), ipc::IpcError> {
        ipc::recv(self.fd())
    }

    pub fn recv_bytes(&self, header: &ipc::IpcHeader, buf: &mut [u8]) -> Result<usize, ipc::IpcError> {
        ipc::recv_bytes(self.fd(), header, buf)
    }

    pub fn read(&self, buf: &mut [u8]) -> Result<usize, toyos_abi::syscall::SyscallError> {
        toyos_abi::syscall::read(self.fd(), buf)
    }

    pub fn write(&self, buf: &[u8]) -> Result<usize, toyos_abi::syscall::SyscallError> {
        toyos_abi::syscall::write(self.fd(), buf)
    }

    pub fn read_nonblock(&self, buf: &mut [u8]) -> Result<usize, toyos_abi::syscall::SyscallError> {
        toyos_abi::syscall::read_nonblock(self.fd(), buf)
    }

    pub fn write_nonblock(&self, buf: &[u8]) -> Result<usize, toyos_abi::syscall::SyscallError> {
        toyos_abi::syscall::write_nonblock(self.fd(), buf)
    }
}

/// A claimed hardware device. Created by [`device::open_keyboard`] etc.
pub struct Device(pub(crate) Handle);

impl Device {
    pub fn fd(&self) -> Fd { self.0.0 }

    pub fn read(&self, buf: &mut [u8]) -> Result<usize, toyos_abi::syscall::SyscallError> {
        toyos_abi::syscall::read(self.fd(), buf)
    }
}

/// A kernel pipe endpoint. Created by [`pipe::open_by_id`].
pub struct Pipe(pub(crate) Handle);

impl Pipe {
    pub fn fd(&self) -> Fd { self.0.0 }

    pub fn read(&self, buf: &mut [u8]) -> Result<usize, toyos_abi::syscall::SyscallError> {
        toyos_abi::syscall::read(self.fd(), buf)
    }

    pub fn write(&self, buf: &[u8]) -> Result<usize, toyos_abi::syscall::SyscallError> {
        toyos_abi::syscall::write(self.fd(), buf)
    }

    pub fn read_nonblock(&self, buf: &mut [u8]) -> Result<usize, toyos_abi::syscall::SyscallError> {
        toyos_abi::syscall::read_nonblock(self.fd(), buf)
    }

    pub fn write_nonblock(&self, buf: &[u8]) -> Result<usize, toyos_abi::syscall::SyscallError> {
        toyos_abi::syscall::write_nonblock(self.fd(), buf)
    }

    pub fn pipe_map(&self) -> Result<*mut u8, toyos_abi::syscall::SyscallError> {
        toyos_abi::syscall::pipe_map(self.fd())
    }

    pub fn pipe_id(&self) -> Result<u64, toyos_abi::syscall::SyscallError> {
        toyos_abi::syscall::pipe_id(self.fd())
    }
}
