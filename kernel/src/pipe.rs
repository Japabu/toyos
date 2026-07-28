use crate::mm::pmm;

use toyos_abi::ring::{RingHeader, RING_READER_CLOSED, RING_WRITER_CLOSED};

use alloc::vec::Vec;

use crate::mm::PAGE_2M;
use crate::io_uring::RingId;
use crate::process::Pid;
use crate::id_map::{IdKey, IdMap};
use crate::sync::Lock;
use crate::DirectMap;

// ---------------------------------------------------------------------------
// PipeId — raw identifier, Copy, used internally for lookups and in
// ProcessState. Does NOT carry a refcount. Not public outside the kernel.
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// PipeReader / PipeWriter — owned refcounted references.
// Creation bumps, Drop decrements. Clone bumps. No other way to get one.
// ---------------------------------------------------------------------------

/// Owned reader reference to a pipe. Bumps reader refcount on creation/clone,
/// decrements on drop. Like Arc but for pipe reader slots.
pub struct PipeReader(PipeId);

/// Owned writer reference to a pipe. Same semantics as PipeReader but for writers.
pub struct PipeWriter(PipeId);

impl PipeReader {
    pub fn id(&self) -> PipeId { self.0 }
}

impl PipeWriter {
    pub fn id(&self) -> PipeId { self.0 }
}

impl Clone for PipeReader {
    fn clone(&self) -> Self {
        add_reader(self.0);
        Self(self.0)
    }
}

impl Clone for PipeWriter {
    fn clone(&self) -> Self {
        add_writer(self.0);
        Self(self.0)
    }
}

impl Drop for PipeReader {
    fn drop(&mut self) {
        close_read(self.0);
    }
}

impl Drop for PipeWriter {
    fn drop(&mut self) {
        close_write(self.0);
    }
}

// ---------------------------------------------------------------------------
// Pipe internals — owns physical memory, tracks refcounts only.
// Mapping into user address spaces is managed by the FD layer.
// ---------------------------------------------------------------------------

pub const PIPE_SIZE: usize = PAGE_2M as usize;

struct Pipe {
    page: pmm::PhysPage,
    /// The process that called `create`. `PipeId`s are dense sequential
    /// integers and userland passes raw ones to `SYS_PIPE_OPEN`, so this is
    /// the only thing that distinguishes "a peer handed me this id" from
    /// "I counted up from zero". See `sys_pipe_open`.
    creator: Pid,
    readers: u32,
    writers: u32,
    io_uring_watchers: Vec<RingId>,
    /// An RT thread wrote to this pipe and the boost has not been claimed
    /// yet. The next thread to consume data inherits transient RT priority —
    /// covering readers that were runnable (not blocked) at write time,
    /// which the wake-time boost in `wake_pipe_readers` misses.
    rt_boost_pending: bool,
}

unsafe impl Send for Pipe {}

impl Pipe {
    fn new(creator: Pid) -> Self {
        let page = pmm::alloc_page(pmm::Category::Pipe).expect("pipe: allocation failed");
        RingHeader::init(page.direct_map().as_mut_ptr(), PIPE_SIZE);
        Self { page, creator, readers: 0, writers: 0, io_uring_watchers: Vec::new(), rt_boost_pending: false }
    }

    fn header(&self) -> &RingHeader {
        unsafe { &*self.page.direct_map().as_ptr::<RingHeader>() }
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

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Create a new pipe. Returns owned reader + writer references.
pub fn create(creator: Pid) -> (PipeReader, PipeWriter) {
    let id = with_pipes_mut(|pipes| pipes.insert(Pipe::new(creator)));
    add_reader(id);
    add_writer(id);
    (PipeReader(id), PipeWriter(id))
}

/// The process that created this pipe, or `None` if the id names no pipe.
pub fn creator(id: PipeId) -> Option<Pid> {
    with_pipes(|pipes| pipes.get(id).map(|p| p.creator))
}

/// Open an existing pipe by raw ID (for cross-process pipe sharing).
pub fn open_reader(id: PipeId) -> Option<PipeReader> {
    if !exists(id) { return None; }
    add_reader(id);
    Some(PipeReader(id))
}

pub fn open_writer(id: PipeId) -> Option<PipeWriter> {
    if !exists(id) { return None; }
    add_writer(id);
    Some(PipeWriter(id))
}

pub fn exists(pipe_id: PipeId) -> bool {
    with_pipes(|pipes| pipes.get(pipe_id).is_some())
}

/// Get the physical address of a pipe's ring buffer (for mapping into userland).
pub fn phys_addr(pipe_id: PipeId) -> Option<DirectMap> {
    with_pipes(|pipes| pipes.get(pipe_id).map(|p| p.page.direct_map()))
}

pub fn try_read(pipe_id: PipeId, buf: &mut [u8]) -> Option<usize> {
    let (result, boost) = with_pipes_mut(|pipes| {
        let Some(pipe) = pipes.get_mut(pipe_id) else { return (None, false) };
        let header = pipe.header();
        if header.available() > 0 {
            let n = header.read(buf);
            let boost = pipe.rt_boost_pending;
            pipe.rt_boost_pending = false;
            (Some(n), boost)
        } else if pipe.writers == 0 || header.is_writer_closed() {
            (Some(0), false)
        } else {
            (None, false)
        }
    });
    if boost {
        // Boost-on-consume: outside the PIPES lock — the scheduler takes
        // its own CPU-queue lock.
        crate::scheduler::boost_current_rt_inherited();
    }
    result
}

pub enum PipeWrite {
    Wrote(usize),
    BrokenPipe,
}

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

/// Mark the pipe so the next consumer inherits RT priority. Called by the
/// wake path when the writer is RT (see `Pipe::rt_boost_pending`).
pub fn set_rt_boost_pending(pipe_id: PipeId) {
    with_pipes_mut(|pipes| {
        if let Some(pipe) = pipes.get_mut(pipe_id) {
            pipe.rt_boost_pending = true;
        }
    });
}

// ---------------------------------------------------------------------------
// Internal refcount management (called by PipeReader/PipeWriter)
// ---------------------------------------------------------------------------

fn add_reader(pipe_id: PipeId) {
    with_pipes_mut(|pipes| {
        let pipe = pipes.get_mut(pipe_id).expect("add_reader: pipe not found");
        pipe.readers = pipe.readers.checked_add(1).expect("pipe reader overflow");
        pipe.header().flags.fetch_and(!RING_READER_CLOSED, core::sync::atomic::Ordering::Release);
    });
}

fn add_writer(pipe_id: PipeId) {
    with_pipes_mut(|pipes| {
        let pipe = pipes.get_mut(pipe_id).expect("add_writer: pipe not found");
        pipe.writers = pipe.writers.checked_add(1).expect("pipe writer overflow");
        pipe.header().flags.fetch_and(!RING_WRITER_CLOSED, core::sync::atomic::Ordering::Release);
    });
}

fn close_read(pipe_id: PipeId) {
    let wake_writers = with_pipes_mut(|pipes| {
        let pipe = pipes.get_mut(pipe_id).expect("close_read: pipe not found");
        pipe.readers = pipe.readers.checked_sub(1).expect("pipe reader underflow");
        if pipe.readers == 0 {
            pipe.header().close_reader();
        }
        if pipe.readers == 0 && pipe.writers == 0 {
            let pipe = pipes.remove(pipe_id).unwrap();
            free_pipe(pipe);
            None // pipe freed, no one to wake
        } else if pipe.readers == 0 {
            // Writers need to discover broken pipe
            Some(pipe.io_uring_watchers.clone())
        } else {
            None
        }
    });
    if let Some(watchers) = wake_writers {
        crate::scheduler::wake_pipe_writers(pipe_id);
        if !watchers.is_empty() {
            crate::io_uring::complete_pending_for_event(
                &watchers,
                crate::scheduler::EventSource::PipeWritable(pipe_id),
            );
        }
    }
}

fn close_write(pipe_id: PipeId) {
    let wake_readers = with_pipes_mut(|pipes| {
        let pipe = pipes.get_mut(pipe_id).expect("close_write: pipe not found");
        pipe.writers = pipe.writers.checked_sub(1).expect("pipe writer underflow");
        if pipe.writers == 0 {
            pipe.header().close_writer();
        }
        if pipe.readers == 0 && pipe.writers == 0 {
            let pipe = pipes.remove(pipe_id).unwrap();
            free_pipe(pipe);
            None // pipe freed, no one to wake
        } else if pipe.writers == 0 {
            // Readers need to discover EOF
            Some(pipe.io_uring_watchers.clone())
        } else {
            None
        }
    });
    if let Some(watchers) = wake_readers {
        crate::scheduler::wake_pipe_readers(pipe_id);
        if !watchers.is_empty() {
            crate::io_uring::complete_pending_for_event(
                &watchers,
                crate::scheduler::EventSource::PipeReadable(pipe_id),
            );
        }
    }
}

fn free_pipe(pipe: Pipe) {
    drop(pipe); // PhysPage freed via Drop
}

// ---------------------------------------------------------------------------
// io_uring watcher management
// ---------------------------------------------------------------------------

pub fn add_io_uring_watcher(pipe_id: PipeId, ring_id: RingId) {
    with_pipes_mut(|pipes| {
        if let Some(pipe) = pipes.get_mut(pipe_id) {
            if !pipe.io_uring_watchers.contains(&ring_id) {
                pipe.io_uring_watchers.push(ring_id);
            }
        }
    });
}

pub fn remove_io_uring_watcher(pipe_id: PipeId, ring_id: RingId) {
    with_pipes_mut(|pipes| {
        if let Some(pipe) = pipes.get_mut(pipe_id) {
            pipe.io_uring_watchers.retain(|&id| id != ring_id);
        }
    });
}

pub fn io_uring_watchers(pipe_id: PipeId) -> Vec<RingId> {
    with_pipes(|pipes| {
        pipes.get(pipe_id).map_or(Vec::new(), |p| p.io_uring_watchers.clone())
    })
}
