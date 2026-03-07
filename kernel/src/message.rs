use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::process::Pid;

/// Kernel-side message. Payload bytes are copied from the sender's address space
/// and will be copied into the receiver's user heap during recv.
#[derive(Clone)]
pub struct Message {
    pub sender: Pid,
    pub msg_type: u32,
    pub payload: Vec<u8>,
}

/// Layout of the message struct as seen by userland (passed via syscall).
/// The kernel reads/writes this format at the user-provided pointer.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct UserMessage {
    pub sender: u32,
    pub msg_type: u32,
    pub data: u64,
    pub len: u64,
}

pub struct MessageQueue {
    queue: VecDeque<Message>,
}

const MAX_MESSAGES: usize = 256;

impl MessageQueue {
    pub fn new() -> Self {
        Self {
            queue: VecDeque::new(),
        }
    }

    /// Push a message. Returns false if the queue is full.
    pub fn push(&mut self, msg: Message) -> bool {
        if self.queue.len() >= MAX_MESSAGES {
            return false;
        }
        self.queue.push_back(msg);
        true
    }

    pub fn pop(&mut self) -> Option<Message> {
        self.queue.pop_front()
    }

    pub fn has_messages(&self) -> bool {
        !self.queue.is_empty()
    }
}
