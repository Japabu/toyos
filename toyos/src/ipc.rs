//! Framed IPC over sockets (bidirectional pipe pairs).
//!
//! Wire format: `[u32 msg_type][u32 len][len bytes payload]`.

use toyos_abi::Fd;
use toyos_abi::syscall::{self, SyscallError};
use crate::{AsHandle, Handle};

#[repr(C)]
#[derive(Clone, Copy)]
pub struct IpcHeader {
    pub msg_type: u32,
    pub len: u32,
}

#[derive(Debug)]
pub enum IpcError {
    Disconnected,
    Syscall(SyscallError),
}

// ---------------------------------------------------------------------------
// Connection — typed IPC endpoint
// ---------------------------------------------------------------------------

/// An IPC connection. Created by [`crate::services::accept`] or
/// [`crate::services::connect`].
pub struct Connection(pub(crate) Handle);

impl AsHandle for Connection {
    fn as_handle(&self) -> Fd { self.0.fd() }
}

impl Connection {
    pub fn fd(&self) -> Fd { self.0.fd() }

    pub fn send<T: Copy>(&self, msg_type: u32, payload: &T) -> Result<(), IpcError> {
        send(self.fd(), msg_type, payload)
    }

    pub fn signal(&self, msg_type: u32) -> Result<(), IpcError> {
        signal(self.fd(), msg_type)
    }

    pub fn send_bytes(&self, msg_type: u32, data: &[u8]) -> Result<(), IpcError> {
        send_bytes(self.fd(), msg_type, data)
    }

    pub fn recv_header(&self) -> Result<IpcHeader, IpcError> {
        recv_header(self.fd())
    }

    pub fn recv_payload<T: Copy>(&self, header: &IpcHeader) -> Result<T, IpcError> {
        recv_payload(self.fd(), header)
    }

    pub fn recv<T: Copy>(&self) -> Result<(u32, T), IpcError> {
        recv(self.fd())
    }

    pub fn recv_bytes(&self, header: &IpcHeader, buf: &mut [u8]) -> Result<usize, IpcError> {
        recv_bytes(self.fd(), header, buf)
    }

    pub fn read_nonblock(&self, buf: &mut [u8]) -> Result<usize, SyscallError> {
        self.0.read_nonblock(buf)
    }

    pub fn write_nonblock(&self, buf: &[u8]) -> Result<usize, SyscallError> {
        self.0.write_nonblock(buf)
    }
}

// ---------------------------------------------------------------------------
// Free functions — used by consumers that hold raw Fds (compositor, netd).
// Will become pub(crate) once all callers migrate to Connection methods.
// ---------------------------------------------------------------------------

pub fn send<T: Copy>(fd: Fd, msg_type: u32, payload: &T) -> Result<(), IpcError> {
    let header = IpcHeader { msg_type, len: core::mem::size_of::<T>() as u32 };
    write_all(fd, as_bytes(&header))?;
    write_all(fd, as_bytes(payload))
}

pub fn signal(fd: Fd, msg_type: u32) -> Result<(), IpcError> {
    let header = IpcHeader { msg_type, len: 0 };
    write_all(fd, as_bytes(&header))
}

pub fn send_bytes(fd: Fd, msg_type: u32, data: &[u8]) -> Result<(), IpcError> {
    let header = IpcHeader { msg_type, len: data.len() as u32 };
    write_all(fd, as_bytes(&header))?;
    if !data.is_empty() {
        write_all(fd, data)?;
    }
    Ok(())
}

pub fn recv_header(fd: Fd) -> Result<IpcHeader, IpcError> {
    let mut header = IpcHeader { msg_type: 0, len: 0 };
    read_exact(fd, as_bytes_mut(&mut header))?;
    Ok(header)
}


pub fn recv_payload<T: Copy>(fd: Fd, header: &IpcHeader) -> Result<T, IpcError> {
    let size = core::mem::size_of::<T>();
    assert!(header.len as usize >= size);
    let mut val = unsafe { core::mem::zeroed::<T>() };
    read_exact(fd, as_bytes_mut(&mut val))?;
    skip(fd, header.len as usize - size)?;
    Ok(val)
}

/// Receive header + typed payload in one call.
pub fn recv<T: Copy>(fd: Fd) -> Result<(u32, T), IpcError> {
    let header = recv_header(fd)?;
    let payload = recv_payload(fd, &header)?;
    Ok((header.msg_type, payload))
}

/// Receive raw bytes. Returns the number of valid bytes read.
pub fn recv_bytes(fd: Fd, header: &IpcHeader, buf: &mut [u8]) -> Result<usize, IpcError> {
    let count = (header.len as usize).min(buf.len());
    if count > 0 {
        read_exact(fd, &mut buf[..count])?;
    }
    skip(fd, header.len as usize - count)?;
    Ok(count)
}

fn as_bytes<T>(val: &T) -> &[u8] {
    unsafe { core::slice::from_raw_parts(val as *const T as *const u8, core::mem::size_of::<T>()) }
}

fn as_bytes_mut<T>(val: &mut T) -> &mut [u8] {
    unsafe { core::slice::from_raw_parts_mut(val as *mut T as *mut u8, core::mem::size_of::<T>()) }
}

fn skip(fd: Fd, mut remaining: usize) -> Result<(), IpcError> {
    let mut buf = [0u8; 128];
    while remaining > 0 {
        let chunk = remaining.min(buf.len());
        read_exact(fd, &mut buf[..chunk])?;
        remaining -= chunk;
    }
    Ok(())
}

fn read_exact(fd: Fd, buf: &mut [u8]) -> Result<(), IpcError> {
    let mut offset = 0;
    while offset < buf.len() {
        let n = syscall::read(fd, &mut buf[offset..]).map_err(IpcError::Syscall)?;
        if n == 0 {
            return Err(IpcError::Disconnected);
        }
        offset += n;
    }
    Ok(())
}

fn write_all(fd: Fd, buf: &[u8]) -> Result<(), IpcError> {
    let mut offset = 0;
    while offset < buf.len() {
        let n = syscall::write(fd, &buf[offset..]).map_err(IpcError::Syscall)?;
        offset += n;
    }
    Ok(())
}
