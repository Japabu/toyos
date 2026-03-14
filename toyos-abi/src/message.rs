//! Type-safe kernel message passing between processes.
//!
//! Payloads are owned by userland: send copies from sender → kernel,
//! recv copies from kernel → a user-provided buffer.

use crate::Pid;
use crate::syscall;

/// Wire format for the send/recv message syscalls.
///
/// For sends, `data`/`len` point to the payload in the sender's address space.
/// For receives, `data`/`len` describe the user-provided buffer where the
/// kernel copied the payload.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RawMessage {
    pub sender: u32,
    pub msg_type: u32,
    pub data: u64,
    pub len: u64,
}

/// Send a typed payload to another process. The kernel copies from `payload`.
pub fn send<T>(target: Pid, msg_type: u32, payload: &T) {
    let msg = RawMessage {
        sender: 0,
        msg_type,
        data: payload as *const T as u64,
        len: core::mem::size_of::<T>() as u64,
    };
    unsafe { syscall::send_msg(target.0 as u64, &msg as *const RawMessage as u64) };
}

/// Send a variable-length byte payload. The kernel copies from `data`.
pub fn send_bytes(target: Pid, msg_type: u32, data: &[u8]) {
    let msg = RawMessage {
        sender: 0,
        msg_type,
        data: data.as_ptr() as u64,
        len: data.len() as u64,
    };
    unsafe { syscall::send_msg(target.0 as u64, &msg as *const RawMessage as u64) };
}

/// Send a message with no payload.
pub fn signal(target: Pid, msg_type: u32) {
    let msg = RawMessage { sender: 0, msg_type, data: 0, len: 0 };
    unsafe { syscall::send_msg(target.0 as u64, &msg as *const RawMessage as u64) };
}

/// Receive the next message into `buf` (blocks if queue is empty).
///
/// Returns `(raw, actual_len)` where `actual_len` is the original payload size
/// (may exceed `buf.len()` if the message was truncated).
pub fn recv_into(buf: &mut [u8]) -> (RawMessage, usize) {
    let mut raw = RawMessage { sender: 0, msg_type: 0, data: 0, len: 0 };
    unsafe {
        syscall::recv_msg(
            &mut raw as *mut RawMessage as u64,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
        );
    }
    let actual_len = raw.len as usize;
    (raw, actual_len)
}

/// A received message with payload on the stack. For `no_std` crates that
/// can't allocate. Buffer is 256 bytes — sufficient for IPC control messages.
pub struct ReceivedMessage {
    sender: u32,
    msg_type: u32,
    buf: [u8; 256],
    len: usize,
}

impl ReceivedMessage {
    /// Sender PID.
    pub fn sender(&self) -> u32 {
        self.sender
    }

    /// Message type tag.
    pub fn msg_type(&self) -> u32 {
        self.msg_type
    }

    /// Payload as a byte slice.
    pub fn bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    /// Raw pointer to the payload buffer.
    pub fn data_ptr(&self) -> *const u8 {
        self.buf.as_ptr()
    }

    /// Payload size in bytes.
    pub fn data_len(&self) -> usize {
        self.len
    }

    /// Read the typed payload.
    ///
    /// # Panics
    /// Panics if the payload is smaller than `size_of::<T>()`.
    pub fn payload<T: Copy>(&self) -> T {
        let expected = core::mem::size_of::<T>();
        if expected == 0 {
            return unsafe { core::mem::zeroed() };
        }
        assert!(
            self.len >= expected,
            "message payload too small: got {}, expected {expected}",
            self.len,
        );
        unsafe { core::ptr::read_unaligned(self.buf.as_ptr() as *const T) }
    }
}

/// Receive the next message (blocks if queue is empty).
/// Uses a 256-byte stack buffer — suitable for IPC control messages.
pub fn recv() -> ReceivedMessage {
    let mut msg = ReceivedMessage {
        sender: 0,
        msg_type: 0,
        buf: [0u8; 256],
        len: 0,
    };
    let (raw, actual_len) = recv_into(&mut msg.buf);
    msg.sender = raw.sender;
    msg.msg_type = raw.msg_type;
    msg.len = actual_len.min(256);
    msg
}
