use alloc::collections::VecDeque;

pub use toyos_abi::message::Message;

pub struct QueueFull;

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

    /// Push a message. Returns Err if the queue is full.
    pub fn push(&mut self, msg: Message) -> Result<(), QueueFull> {
        if self.queue.len() >= MAX_MESSAGES {
            return Err(QueueFull);
        }
        self.queue.push_back(msg);
        Ok(())
    }

    pub fn pop(&mut self) -> Option<Message> {
        self.queue.pop_front()
    }

    pub fn has_messages(&self) -> bool {
        !self.queue.is_empty()
    }
}
