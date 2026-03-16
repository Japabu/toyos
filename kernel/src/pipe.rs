use alloc::alloc::{alloc_zeroed, dealloc};
use alloc::vec::Vec;
use core::alloc::Layout;

use toyos_abi::ring::RingHeader;

use crate::arch::paging::{self, PAGE_2M};
use crate::id_map::{IdKey, IdMap};
use crate::sync::Lock;
use crate::PhysAddr;

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

const PIPE_SIZE: usize = PAGE_2M as usize;

struct Pipe {
    phys_addr: PhysAddr,
    layout: Layout,
    readers: u32,
    writers: u32,
    /// PML4 pointers of processes that have this pipe mapped (for unmap on free).
    mapped_in: Vec<*mut u64>,
}

// SAFETY: mapped_in contains raw PML4 pointers — physical addresses valid across CPUs.
// Access is serialized by the PIPES lock.
unsafe impl Send for Pipe {}

impl Pipe {
    fn new() -> Self {
        let layout = Layout::from_size_align(PIPE_SIZE, PIPE_SIZE).unwrap();
        let ptr = unsafe { alloc_zeroed(layout) };
        assert!(!ptr.is_null(), "pipe: allocation failed");
        RingHeader::init(ptr, PIPE_SIZE);
        Self {
            phys_addr: PhysAddr::from_ptr(ptr),
            layout,
            readers: 1,
            writers: 1,
            mapped_in: Vec::new(),
        }
    }

    fn header(&self) -> &RingHeader {
        unsafe { &*self.phys_addr.as_ptr::<RingHeader>() }
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

pub fn exists(pipe_id: PipeId) -> bool {
    with_pipes(|pipes| pipes.get(pipe_id).is_some())
}

/// Returns bytes read, 0 for EOF, None if would block.
pub fn try_read(pipe_id: PipeId, buf: &mut [u8]) -> Option<usize> {
    with_pipes(|pipes| {
        let pipe = pipes.get(pipe_id)?;
        let header = pipe.header();
        if header.available() > 0 {
            Some(header.read(buf))
        } else if pipe.writers == 0 || header.is_writer_closed() {
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
    with_pipes(|pipes| {
        let pipe = pipes.get(pipe_id)?;
        let header = pipe.header();
        if pipe.readers == 0 || header.is_reader_closed() {
            Some(PipeWrite::BrokenPipe)
        } else if header.space() > 0 {
            Some(PipeWrite::Wrote(header.write(buf)))
        } else {
            None
        }
    })
}

pub fn has_data(pipe_id: PipeId) -> bool {
    with_pipes(|pipes| {
        pipes.get(pipe_id).map_or(false, |p| {
            p.header().available() > 0 || p.writers == 0 || p.header().is_writer_closed()
        })
    })
}

pub fn has_space(pipe_id: PipeId) -> bool {
    with_pipes(|pipes| {
        pipes.get(pipe_id).map_or(false, |p| {
            p.header().space() > 0 || p.readers == 0 || p.header().is_reader_closed()
        })
    })
}

pub fn all_empty() -> bool {
    with_pipes(|pipes| pipes.iter().all(|(_, pipe)| pipe.header().available() == 0))
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

fn free_pipe(pipe: Pipe) {
    // Unmap USER bit from all processes' page tables before freeing memory.
    // Without this, the freed pages retain USER bit, and if the heap reuses
    // this memory for kernel stacks, SMAP violations occur during context_switch.
    for &pml4 in &pipe.mapped_in {
        paging::unmap_user(pml4, pipe.phys_addr, PIPE_SIZE as u64);
    }
    unsafe { dealloc(pipe.phys_addr.as_mut_ptr(), pipe.layout); }
}

pub fn close_read(pipe_id: PipeId) {
    with_pipes_mut(|pipes| {
        let pipe = pipes.get_mut(pipe_id).expect("close_read: pipe not found");
        pipe.readers = pipe.readers.checked_sub(1).expect("pipe reader underflow");
        if pipe.readers == 0 {
            pipe.header().close_reader();
        }
        if pipe.readers == 0 && pipe.writers == 0 {
            let pipe = pipes.remove(pipe_id).unwrap();
            free_pipe(pipe);
        }
    });
}

pub fn close_write(pipe_id: PipeId) {
    with_pipes_mut(|pipes| {
        let pipe = pipes.get_mut(pipe_id).expect("close_write: pipe not found");
        pipe.writers = pipe.writers.checked_sub(1).expect("pipe writer underflow");
        if pipe.writers == 0 {
            pipe.header().close_writer();
        }
        if pipe.readers == 0 && pipe.writers == 0 {
            let pipe = pipes.remove(pipe_id).unwrap();
            free_pipe(pipe);
        }
    });
}

/// Remove a PML4 from all pipes' mapped_in lists (called during process exit).
/// Does NOT unmap — the page tables are about to be freed anyway.
pub fn cleanup_pml4(pml4: *mut u64) {
    with_pipes_mut(|pipes| {
        for (_, pipe) in pipes.iter_mut() {
            pipe.mapped_in.retain(|p| *p != pml4);
        }
    });
}

/// Map a pipe's shared memory into a process's address space.
/// Returns the physical address (user processes see this as their virtual address).
pub fn map_into(pipe_id: PipeId, pml4: *mut u64) -> Option<u64> {
    with_pipes_mut(|pipes| {
        let pipe = pipes.get_mut(pipe_id)?;
        paging::map_user_in(pml4, pipe.phys_addr, PIPE_SIZE as u64);
        if !pipe.mapped_in.contains(&pml4) {
            pipe.mapped_in.push(pml4);
        }
        Some(pipe.phys_addr.raw())
    })
}
