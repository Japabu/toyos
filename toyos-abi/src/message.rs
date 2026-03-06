//! Low-level typed message passing between processes.
//!
//! This module provides the wire format and helpers for kernel IPC messages.
//! It mirrors `std::os::toyos::message` but works in `no_std` contexts
//! (like mio/tokio forks) that can't depend on std.

use crate::syscall;

/// Wire format for messages passed through the kernel message queue.
/// Must match `kernel::message::UserMessage` and `std::os::toyos::message::Message`.
#[repr(C)]
pub struct Message {
    pub sender: u32,
    pub msg_type: u32,
    pub data: u64,
    pub len: u64,
}

impl Message {
    /// Create a message with a typed payload. The payload is copied to a
    /// syscall-allocated buffer whose pointer is stored in `data`.
    pub fn new<T: Copy>(msg_type: u32, payload: &T) -> Self {
        let len = core::mem::size_of::<T>();
        let data = if len > 0 {
            let ptr = syscall::alloc(len, 8);
            unsafe {
                core::ptr::copy_nonoverlapping(
                    payload as *const T as *const u8,
                    ptr,
                    len,
                );
            }
            ptr as u64
        } else {
            0
        };
        Self { sender: 0, msg_type, data, len: len as u64 }
    }

    /// Create a message with no payload.
    pub fn signal(msg_type: u32) -> Self {
        Self { sender: 0, msg_type, data: 0, len: 0 }
    }

    /// Extract the typed payload. Frees the kernel-allocated payload buffer.
    ///
    /// # Panics
    /// Panics if the payload is smaller than `size_of::<T>()`.
    pub fn take_payload<T: Copy>(&self) -> T {
        let expected = core::mem::size_of::<T>();
        if expected == 0 {
            self.free_payload();
            return unsafe { core::mem::zeroed() };
        }
        assert!(
            self.len as usize >= expected,
            "message payload too small: got {}, expected {expected}",
            self.len,
        );
        let value = unsafe { core::ptr::read(self.data as *const T) };
        self.free_payload();
        value
    }

    /// Free the kernel-allocated payload buffer without reading it.
    pub fn free_payload(&self) {
        if self.data != 0 && self.len != 0 {
            syscall::free(
                core::ptr::with_exposed_provenance_mut(self.data as usize),
                self.len as usize,
                1,
            );
        }
    }
}

/// Send a message to another process. Frees the sender's payload allocation
/// after the kernel has copied it.
pub fn send(target_pid: u64, msg: Message) {
    syscall::send_msg(target_pid, &msg as *const Message as u64);
    // The kernel copied the payload; free our copy
    if msg.data != 0 && msg.len != 0 {
        syscall::free(
            core::ptr::with_exposed_provenance_mut(msg.data as usize),
            msg.len as usize,
            8,
        );
    }
    core::mem::forget(msg);
}

/// Receive the next message (blocks if queue is empty).
pub fn recv() -> Message {
    let mut msg = Message { sender: 0, msg_type: 0, data: 0, len: 0 };
    syscall::recv_msg(&mut msg as *mut Message as u64);
    msg
}
