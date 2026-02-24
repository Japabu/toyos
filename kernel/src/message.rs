use alloc::collections::VecDeque;

#[repr(C)]
#[derive(Clone)]
pub struct Message {
    pub sender: u32,
    pub msg_type: u32,
    pub data: u64,
    pub len: u64,
}

pub struct MessageQueue {
    queue: VecDeque<Message>,
}

impl MessageQueue {
    pub fn new() -> Self {
        Self {
            queue: VecDeque::new(),
        }
    }

    pub fn push(&mut self, msg: Message) {
        self.queue.push_back(msg);
    }

    pub fn pop(&mut self) -> Option<Message> {
        self.queue.pop_front()
    }

    pub fn has_messages(&self) -> bool {
        !self.queue.is_empty()
    }
}
