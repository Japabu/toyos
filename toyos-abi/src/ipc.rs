//! Framed IPC over sockets (bidirectional pipe pairs).
//!
//! Wire format: `[u32 msg_type][u32 len][len bytes payload]`.

use crate::Fd;
use crate::syscall::{self, SyscallError};

#[repr(C)]
#[derive(Clone, Copy)]
pub struct IpcHeader {
    pub msg_type: u32,
    pub len: u32,
}

pub fn send<T: Copy>(fd: Fd, msg_type: u32, payload: &T) -> Result<(), SyscallError> {
    let header = IpcHeader { msg_type, len: core::mem::size_of::<T>() as u32 };
    write_all(fd, as_bytes(&header))?;
    write_all(fd, as_bytes(payload))
}

pub fn signal(fd: Fd, msg_type: u32) -> Result<(), SyscallError> {
    let header = IpcHeader { msg_type, len: 0 };
    write_all(fd, as_bytes(&header))
}

pub fn send_bytes(fd: Fd, msg_type: u32, data: &[u8]) -> Result<(), SyscallError> {
    let header = IpcHeader { msg_type, len: data.len() as u32 };
    write_all(fd, as_bytes(&header))?;
    if !data.is_empty() {
        write_all(fd, data)?;
    }
    Ok(())
}

pub fn recv_header(fd: Fd) -> IpcHeader {
    let mut header = IpcHeader { msg_type: 0, len: 0 };
    read_exact(fd, as_bytes_mut(&mut header));
    header
}

pub fn recv_payload<T: Copy>(fd: Fd, header: &IpcHeader) -> T {
    let size = core::mem::size_of::<T>();
    assert!(header.len as usize >= size);
    let mut val = unsafe { core::mem::zeroed::<T>() };
    read_exact(fd, as_bytes_mut(&mut val));
    skip(fd, header.len as usize - size);
    val
}

/// Receive header + typed payload in one call.
pub fn recv<T: Copy>(fd: Fd) -> (u32, T) {
    let header = recv_header(fd);
    let payload = recv_payload(fd, &header);
    (header.msg_type, payload)
}

/// Receive raw bytes. Returns the number of valid bytes read.
pub fn recv_bytes(fd: Fd, header: &IpcHeader, buf: &mut [u8]) -> usize {
    let count = (header.len as usize).min(buf.len());
    if count > 0 {
        read_exact(fd, &mut buf[..count]);
    }
    skip(fd, header.len as usize - count);
    count
}

fn as_bytes<T>(val: &T) -> &[u8] {
    unsafe { core::slice::from_raw_parts(val as *const T as *const u8, core::mem::size_of::<T>()) }
}

fn as_bytes_mut<T>(val: &mut T) -> &mut [u8] {
    unsafe { core::slice::from_raw_parts_mut(val as *mut T as *mut u8, core::mem::size_of::<T>()) }
}

fn skip(fd: Fd, mut remaining: usize) {
    let mut buf = [0u8; 128];
    while remaining > 0 {
        let chunk = remaining.min(buf.len());
        read_exact(fd, &mut buf[..chunk]);
        remaining -= chunk;
    }
}

fn read_exact(fd: Fd, buf: &mut [u8]) {
    let mut offset = 0;
    while offset < buf.len() {
        let n = syscall::read(fd, &mut buf[offset..]).expect("ipc: read failed");
        assert!(n > 0, "ipc: unexpected EOF");
        offset += n;
    }
}

fn write_all(fd: Fd, buf: &[u8]) -> Result<(), SyscallError> {
    let mut offset = 0;
    while offset < buf.len() {
        let n = syscall::write(fd, &buf[offset..])?;
        offset += n;
    }
    Ok(())
}
