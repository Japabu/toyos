//! Type-safe kernel message passing between processes.
//!
//! Zero-copy send, auto-free receive. Works in `no_std` contexts.

use crate::syscall;

/// A received message. Payload buffer is kernel-allocated and freed on drop.
///
/// Also used as the `#[repr(C)]` wire format for syscalls — matches
/// `kernel::message::UserMessage` and `std::os::toyos::message::Message`.
#[repr(C)]
pub struct ReceivedMessage {
    pub sender: u32,
    pub msg_type: u32,
    data: u64,
    len: u64,
}

impl ReceivedMessage {
    /// Read the typed payload. Buffer is freed when this message drops.
    ///
    /// # Panics
    /// Panics if the payload is smaller than `size_of::<T>()`.
    pub fn payload<T: Copy>(&self) -> T {
        let expected = core::mem::size_of::<T>();
        if expected == 0 {
            return unsafe { core::mem::zeroed() };
        }
        assert!(
            self.len as usize >= expected,
            "message payload too small: got {}, expected {expected}",
            self.len,
        );
        unsafe { core::ptr::read(self.data as *const T) }
    }
}

impl Drop for ReceivedMessage {
    fn drop(&mut self) {
        if self.data != 0 && self.len != 0 {
            syscall::free(
                core::ptr::with_exposed_provenance_mut(self.data as usize),
                self.len as usize,
                1,
            );
        }
    }
}

/// Send a typed payload to another process. The kernel copies directly from
/// `payload` during the syscall — zero allocation, zero copy.
pub fn send<T>(target_pid: u64, msg_type: u32, payload: &T) {
    let msg = ReceivedMessage {
        sender: 0,
        msg_type,
        data: payload as *const T as u64,
        len: core::mem::size_of::<T>() as u64,
    };
    syscall::send_msg(target_pid, &msg as *const ReceivedMessage as u64);
    // Don't drop — data points to caller's stack, not a kernel allocation
    core::mem::forget(msg);
}

/// Send a message with no payload.
pub fn signal(target_pid: u64, msg_type: u32) {
    let msg = ReceivedMessage { sender: 0, msg_type, data: 0, len: 0 };
    syscall::send_msg(target_pid, &msg as *const ReceivedMessage as u64);
    core::mem::forget(msg);
}

/// Receive the next message (blocks if queue is empty).
pub fn recv() -> ReceivedMessage {
    let mut msg = ReceivedMessage { sender: 0, msg_type: 0, data: 0, len: 0 };
    syscall::recv_msg(&mut msg as *mut ReceivedMessage as u64);
    msg
}
