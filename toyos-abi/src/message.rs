//! Fixed-size kernel message passing between processes.
//!
//! Messages are 128-byte register windows. IPC is for control;
//! use pipes or shared memory for bulk data.

use crate::syscall;
use crate::Pid;

/// Fixed-size message struct shared between kernel and userland.
/// 128 bytes total — copied atomically by the kernel on send/recv.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Message {
    /// Sender PID (set by kernel on recv, ignored on send).
    pub sender: u32,
    /// Application-defined message type tag.
    pub msg_type: u32,
    /// Number of valid bytes in `data` (0..=116).
    pub len: u32,
    /// Inline payload.
    pub data: [u8; 116],
}

const _: () = assert!(core::mem::size_of::<Message>() == 128);

impl Message {
    pub fn new(msg_type: u32) -> Self {
        Self { sender: 0, msg_type, len: 0, data: [0; 116] }
    }

    /// Read a typed payload from the data field.
    ///
    /// # Panics
    /// Panics if `len` is smaller than `size_of::<T>()`.
    pub fn payload<T: Copy>(&self) -> T {
        let size = core::mem::size_of::<T>();
        assert!(
            self.len as usize >= size,
            "message payload too small: got {}, expected {size}",
            self.len,
        );
        unsafe { core::ptr::read_unaligned(self.data.as_ptr() as *const T) }
    }

    /// Valid payload as a byte slice.
    pub fn bytes(&self) -> &[u8] {
        &self.data[..self.len as usize]
    }
}

/// Maximum inline payload size.
pub const MAX_PAYLOAD: usize = 116;

/// Send a typed payload to another process.
pub fn send<T: Copy>(target: Pid, msg_type: u32, payload: &T) {
    let size = core::mem::size_of::<T>();
    assert!(size <= MAX_PAYLOAD, "payload too large for message, use pipes/shm");
    let mut msg = Message::new(msg_type);
    msg.len = size as u32;
    unsafe {
        core::ptr::copy_nonoverlapping(
            payload as *const T as *const u8,
            msg.data.as_mut_ptr(),
            size,
        );
    }
    unsafe { syscall::send_msg(target.0 as u64, &msg as *const Message as u64) };
}

/// Send raw bytes inline. Panics if `bytes.len() > 116`.
pub fn send_bytes(target: Pid, msg_type: u32, bytes: &[u8]) {
    assert!(bytes.len() <= MAX_PAYLOAD, "data too large for message, use pipes/shm");
    let mut msg = Message::new(msg_type);
    msg.len = bytes.len() as u32;
    msg.data[..bytes.len()].copy_from_slice(bytes);
    unsafe { syscall::send_msg(target.0 as u64, &msg as *const Message as u64) };
}

/// Send a message with no payload.
pub fn signal(target: Pid, msg_type: u32) {
    let msg = Message::new(msg_type);
    unsafe { syscall::send_msg(target.0 as u64, &msg as *const Message as u64) };
}

/// Receive the next message (blocks if queue is empty).
pub fn recv() -> Message {
    let mut msg = Message::new(0);
    unsafe { syscall::recv_msg(&mut msg as *mut Message as u64) };
    msg
}
