use alloc::collections::VecDeque;

use crate::id_map::IdMap;
use crate::sync::Lock;

const PIPE_BUF_SIZE: usize = 4096;

struct Pipe {
    buffer: VecDeque<u8>,
    readers: u32,
    writers: u32,
}

impl Pipe {
    fn new() -> Self {
        Self {
            buffer: VecDeque::with_capacity(PIPE_BUF_SIZE),
            readers: 1,
            writers: 1,
        }
    }

    fn available(&self) -> usize {
        self.buffer.len()
    }

    fn space(&self) -> usize {
        PIPE_BUF_SIZE - self.buffer.len()
    }

    fn read(&mut self, buf: &mut [u8]) -> usize {
        let count = buf.len().min(self.available());
        for b in &mut buf[..count] {
            *b = self.buffer.pop_front().unwrap();
        }
        count
    }

    fn write(&mut self, buf: &[u8]) -> usize {
        let count = buf.len().min(self.space());
        self.buffer.extend(&buf[..count]);
        count
    }
}

static PIPES: Lock<Option<IdMap<usize, Pipe>>> = Lock::new(None);

fn pipes() -> &'static IdMap<usize, Pipe> {
    unsafe { &*PIPES.data_ptr() }.as_ref().expect("pipes not initialized")
}

fn pipes_mut() -> &'static mut IdMap<usize, Pipe> {
    unsafe { &mut *PIPES.data_ptr() }.as_mut().expect("pipes not initialized")
}

pub fn init() {
    *PIPES.lock() = Some(IdMap::new());
}

pub fn create() -> usize {
    pipes_mut().insert(Pipe::new())
}

/// Returns bytes read, 0 for EOF, None if would block.
pub fn try_read(pipe_id: usize, buf: &mut [u8]) -> Option<usize> {
    let pipe = pipes_mut().get_mut(pipe_id)?;
    if pipe.available() > 0 {
        Some(pipe.read(buf))
    } else if pipe.writers == 0 {
        Some(0)
    } else {
        None
    }
}

/// Returns bytes written, usize::MAX for broken pipe, None if would block.
pub fn try_write(pipe_id: usize, buf: &[u8]) -> Option<usize> {
    let pipe = pipes_mut().get_mut(pipe_id)?;
    if pipe.readers == 0 {
        Some(usize::MAX)
    } else if pipe.space() > 0 {
        Some(pipe.write(buf))
    } else {
        None
    }
}

pub fn has_data(pipe_id: usize) -> bool {
    pipes()
        .get(pipe_id)
        .map_or(false, |p| p.available() > 0 || p.writers == 0)
}

pub fn all_empty() -> bool {
    pipes().iter().all(|(_, pipe)| pipe.available() == 0)
}

pub fn add_reader(pipe_id: usize) {
    if let Some(pipe) = pipes_mut().get_mut(pipe_id) {
        pipe.readers += 1;
    }
}

pub fn add_writer(pipe_id: usize) {
    if let Some(pipe) = pipes_mut().get_mut(pipe_id) {
        pipe.writers += 1;
    }
}

pub fn close_read(pipe_id: usize) {
    if let Some(pipe) = pipes_mut().get_mut(pipe_id) {
        pipe.readers -= 1;
        if pipe.readers == 0 && pipe.writers == 0 {
            pipes_mut().remove(pipe_id);
        }
    }
}

pub fn close_write(pipe_id: usize) {
    if let Some(pipe) = pipes_mut().get_mut(pipe_id) {
        pipe.writers -= 1;
        if pipe.readers == 0 && pipe.writers == 0 {
            pipes_mut().remove(pipe_id);
        }
    }
}
