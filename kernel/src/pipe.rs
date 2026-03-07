use alloc::collections::VecDeque;

use crate::id_map::{IdKey, IdMap};
use crate::sync::Lock;

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct PipeId(usize);

impl PipeId {
    pub fn raw(self) -> usize { self.0 }
    pub fn from_raw(v: usize) -> Self { Self(v) }
}

impl core::ops::Add for PipeId {
    type Output = Self;
    fn add(self, rhs: Self) -> Self { PipeId(self.0 + rhs.0) }
}

impl IdKey for PipeId {
    const ZERO: Self = PipeId(0);
    const ONE: Self = PipeId(1);
}

const DEFAULT_PIPE_CAPACITY: usize = 4096;

struct Pipe {
    buffer: VecDeque<u8>,
    capacity: usize,
    readers: u32,
    writers: u32,
}

impl Pipe {
    fn new() -> Self {
        Self::with_capacity(DEFAULT_PIPE_CAPACITY)
    }

    fn with_capacity(capacity: usize) -> Self {
        Self {
            buffer: VecDeque::with_capacity(capacity),
            capacity,
            readers: 1,
            writers: 1,
        }
    }

    fn available(&self) -> usize {
        self.buffer.len()
    }

    fn space(&self) -> usize {
        self.capacity - self.buffer.len()
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

static PIPES: Lock<Option<IdMap<PipeId, Pipe>>> = Lock::new(None);

fn with_pipes<R>(f: impl FnOnce(&IdMap<PipeId, Pipe>) -> R) -> R {
    let guard = PIPES.lock();
    f(guard.as_ref().expect("pipes not initialized"))
}

fn with_pipes_mut<R>(f: impl FnOnce(&mut IdMap<PipeId, Pipe>) -> R) -> R {
    let mut guard = PIPES.lock();
    f(guard.as_mut().expect("pipes not initialized"))
}

pub fn init() {
    *PIPES.lock() = Some(IdMap::new());
}

#[must_use]
pub fn create() -> PipeId {
    with_pipes_mut(|pipes| pipes.insert(Pipe::new()))
}

#[must_use]
pub fn create_with_capacity(capacity: usize) -> PipeId {
    with_pipes_mut(|pipes| pipes.insert(Pipe::with_capacity(capacity)))
}

pub fn exists(pipe_id: PipeId) -> bool {
    with_pipes(|pipes| pipes.get(pipe_id).is_some())
}

/// Returns bytes read, 0 for EOF, None if would block.
pub fn try_read(pipe_id: PipeId, buf: &mut [u8]) -> Option<usize> {
    with_pipes_mut(|pipes| {
        let pipe = pipes.get_mut(pipe_id)?;
        if pipe.available() > 0 {
            Some(pipe.read(buf))
        } else if pipe.writers == 0 {
            Some(0)
        } else {
            None
        }
    })
}

pub enum PipeWrite {
    Wrote(usize),
    BrokenPipe,
}

/// Returns Wrote(n), BrokenPipe, or None if would block.
pub fn try_write(pipe_id: PipeId, buf: &[u8]) -> Option<PipeWrite> {
    with_pipes_mut(|pipes| {
        let pipe = pipes.get_mut(pipe_id)?;
        if pipe.readers == 0 {
            Some(PipeWrite::BrokenPipe)
        } else if pipe.space() > 0 {
            Some(PipeWrite::Wrote(pipe.write(buf)))
        } else {
            None
        }
    })
}

pub fn has_data(pipe_id: PipeId) -> bool {
    with_pipes(|pipes| {
        pipes.get(pipe_id).map_or(false, |p| p.available() > 0 || p.writers == 0)
    })
}

pub fn has_space(pipe_id: PipeId) -> bool {
    with_pipes(|pipes| {
        pipes.get(pipe_id).map_or(false, |p| p.space() > 0 || p.readers == 0)
    })
}

pub fn all_empty() -> bool {
    with_pipes(|pipes| pipes.iter().all(|(_, pipe)| pipe.available() == 0))
}

pub fn add_reader(pipe_id: PipeId) {
    with_pipes_mut(|pipes| {
        let pipe = pipes.get_mut(pipe_id).expect("add_reader: pipe not found");
        pipe.readers = pipe.readers.checked_add(1).expect("pipe reader overflow");
    });
}

pub fn add_writer(pipe_id: PipeId) {
    with_pipes_mut(|pipes| {
        let pipe = pipes.get_mut(pipe_id).expect("add_writer: pipe not found");
        pipe.writers = pipe.writers.checked_add(1).expect("pipe writer overflow");
    });
}

pub fn close_read(pipe_id: PipeId) {
    with_pipes_mut(|pipes| {
        let pipe = pipes.get_mut(pipe_id).expect("close_read: pipe not found");
        pipe.readers = pipe.readers.checked_sub(1).expect("pipe reader underflow");
        if pipe.readers == 0 && pipe.writers == 0 {
            pipes.remove(pipe_id);
        }
    });
}

pub fn close_write(pipe_id: PipeId) {
    with_pipes_mut(|pipes| {
        let pipe = pipes.get_mut(pipe_id).expect("close_write: pipe not found");
        pipe.writers = pipe.writers.checked_sub(1).expect("pipe writer underflow");
        if pipe.readers == 0 && pipe.writers == 0 {
            pipes.remove(pipe_id);
        }
    });
}
