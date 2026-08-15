use crate::mm::pmm;

use toyos_abi::ring::Ring;

use alloc::sync::Arc;
use alloc::vec::Vec;

use toyos_sched::task::WaitClass;

use crate::mm::PAGE_2M;
use crate::sched::payload::KWaitQueue;
use crate::sched::waitqs::new_queue;
use crate::io_uring::RingId;
use crate::id_map::{IdKey, IdMap};
use crate::sync::Lock;
use crate::user_ptr::{UserBytes, UserBytesMut};
use crate::DirectMap;

// PipeId — raw identifier, Copy, used internally for lookups and in
// ProcessState. Does NOT carry a refcount. Not public outside the kernel.

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct PipeId(usize);

impl PipeId {
}

impl core::ops::Add for PipeId {
    type Output = Self;
    fn add(self, rhs: Self) -> Self { PipeId(self.0 + rhs.0) }
}

impl IdKey for PipeId {
    const ZERO: Self = PipeId(0);
    const ONE: Self = PipeId(1);
}

pub use handle::{PipeReader, PipeWriter};

/// Owned references to a pipe's two ends.
///
/// The id inside each handle is private to this module, and `pipe.rs` is this
/// module's *parent*, so nothing in the kernel — this file included — can name
/// a pipe as an owned reference except through `acquire`. `acquire` takes the
/// `&mut Pipe` that only a holder of `PIPES` can produce, so finding the pipe
/// and taking the reference that keeps it alive are one acquisition. There is
/// no program point between them at which the last other end can close and
/// free it, which is what an `exists`-then-`add_reader` pair had.
mod handle {
    use super::{close_read, close_write, with_pipes_mut, Pipe, PipeId};

    /// One counted reference to a pipe's read end — `Arc`, for a reader slot.
    pub struct PipeReader(PipeId);

    /// One counted reference to a pipe's write end.
    pub struct PipeWriter(PipeId);

    impl PipeReader {
        /// The only constructor. Taking the reference and counting it are the
        /// same statement, so an uncounted `PipeReader` cannot be written.
        pub(super) fn acquire(pipe: &mut Pipe) -> Self {
            pipe.readers = pipe.readers.checked_add(1).expect("pipe reader overflow");
            pipe.publish_ends();
            Self(pipe.id)
        }

        pub fn id(&self) -> PipeId { self.0 }
    }

    impl PipeWriter {
        pub(super) fn acquire(pipe: &mut Pipe) -> Self {
            pipe.writers = pipe.writers.checked_add(1).expect("pipe writer overflow");
            pipe.publish_ends();
            Self(pipe.id)
        }

        pub fn id(&self) -> PipeId { self.0 }
    }

    /// A live handle proves the count is at least one, which proves the pipe is
    /// in the table — so unlike `open_reader` this cannot fail on a race, and
    /// its `expect` is the kernel-bug assert it looks like.
    impl Clone for PipeReader {
        fn clone(&self) -> Self {
            with_pipes_mut(|pipes| {
                Self::acquire(pipes.get_mut(self.0).expect("a held PipeReader's pipe is in the table"))
            })
        }
    }

    impl Clone for PipeWriter {
        fn clone(&self) -> Self {
            with_pipes_mut(|pipes| {
                Self::acquire(pipes.get_mut(self.0).expect("a held PipeWriter's pipe is in the table"))
            })
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
}

// Pipe internals — owns physical memory, tracks refcounts only.
// Mapping into user address spaces is managed by the FD layer.

pub const PIPE_SIZE: usize = PAGE_2M as usize;

/// A pipe's ring page and the cursors over it.
///
/// The cursors are kernel memory, because `SYS_PIPE_MAP` maps `page` into the
/// process writable: anything read back out of that page is a value the
/// process chose.
struct Backing {
    page: pmm::PhysPage,
    ring: Ring,
}

struct Pipe {
    /// Its own key in `PIPES`. A handle's id comes from here rather than from
    /// the caller of `acquire`, so a handle cannot be built naming a different
    /// pipe than the one whose count it bumped.
    id: PipeId,
    /// `None` until the pipe is first used. A pipe costs 2 MiB and a
    /// connection is two of them, so allocating on `create` charged every
    /// `SYS_CONNECT` 4 MiB of physical memory before either end had sent a
    /// byte — for a pending connection, before the server had even agreed to
    /// the conversation.
    backing: Option<Backing>,
    readers: u32,
    writers: u32,
    io_uring_watchers: Vec<RingId>,
    /// This pipe end's waiter set (spec §8.6). Held by `Arc` so a blocking
    /// site can clone it out from under the table lock and hold it across its
    /// own park — the ticket and the registration borrow the queue, not the
    /// table.
    readers_wq: Arc<KWaitQueue>,
    writers_wq: Arc<KWaitQueue>,
    /// An RT thread wrote to this pipe and the boost has not been claimed
    /// yet. The next thread to consume data inherits transient RT priority —
    /// covering readers that were runnable (not blocked) at write time,
    /// which the wake-time boost in `wake_pipe_readers` misses.
    rt_boost_pending: bool,
}

unsafe impl Send for Pipe {}

impl Pipe {
    fn new(id: PipeId) -> Self {
        Self {
            id,
            backing: None,
            readers: 0,
            writers: 0,
            io_uring_watchers: Vec::new(),
            readers_wq: new_queue(WaitClass::Pipe),
            writers_wq: new_queue(WaitClass::Pipe),
            rt_boost_pending: false,
        }
    }

    /// Allocate the ring page if this is the first use. `None` when physical
    /// memory is exhausted — which userland drives, so it is an error return
    /// and not a panic.
    fn back(&mut self) -> Option<&mut Backing> {
        if self.backing.is_none() {
            let page = pmm::alloc_page(pmm::Category::Pipe)?;
            // SAFETY: a fresh 2 MiB page this `Pipe` owns for as long as the
            // `Ring` addresses it.
            let ring = unsafe { Ring::new(page.direct_map().as_mut_ptr(), PIPE_SIZE) };
            self.backing = Some(Backing { page, ring });
            self.publish_ends();
        }
        self.backing.as_mut()
    }

    /// Republish "is the other end gone?" into the mapped header.
    ///
    /// The kernel never reads those bits back — its own counts decide — so
    /// this is a publication for netd, and it derives from the counts rather
    /// than being toggled alongside them. A pipe that is not backed yet has
    /// nowhere to publish to and picks the bits up when it is.
    fn publish_ends(&mut self) {
        let Some(backing) = self.backing.as_mut() else { return };
        if self.readers == 0 { backing.ring.close_reader() } else { backing.ring.open_reader() }
        if self.writers == 0 { backing.ring.close_writer() } else { backing.ring.open_writer() }
    }

    fn available(&self) -> u32 {
        self.backing.as_ref().map_or(0, |b| b.ring.available())
    }

    /// A pipe with no page yet has its whole capacity free — the allocation
    /// that would make that true is deferred, not refused.
    fn space(&self) -> u32 {
        self.backing.as_ref().map_or(u32::MAX, |b| b.ring.space())
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

/// Create a new pipe. Returns owned reader + writer references.
///
/// Infallible: a pipe with no traffic on it owns no physical memory, so there
/// is nothing here that can be exhausted. The 2 MiB ring page is allocated by
/// the first `try_write` or `map_page`, and *that* is where userland driving
/// physical memory — `SYS_PIPE` or `SYS_CONNECT` in a loop — meets an error
/// return.
pub fn create() -> (PipeReader, PipeWriter) {
    with_pipes_mut(|pipes| {
        let id = pipes.insert_with(Pipe::new);
        let pipe = pipes.get_mut(id).expect("the pipe just inserted");
        let reader = PipeReader::acquire(pipe);
        let writer = PipeWriter::acquire(pipe);
        (reader, writer)
    })
}

/// The pipe's ring page, allocating it if this is its first use.
///
/// `None` when the id names no pipe or its page cannot be allocated — the
/// caller holds a descriptor for it, which rules the first out, so what
/// reaches userland from here is physical memory exhaustion.
pub fn map_page(pipe_id: PipeId) -> Option<DirectMap> {
    with_pipes_mut(|pipes| Some(pipes.get_mut(pipe_id)?.back()?.page.direct_map()))
}

pub fn try_read(pipe_id: PipeId, buf: &mut UserBytesMut) -> Option<usize> {
    let (result, boost) = with_pipes_mut(|pipes| {
        let Some(pipe) = pipes.get_mut(pipe_id) else { return (None, false) };
        if pipe.available() > 0 {
            let len = buf.len();
            let n = pipe
                .backing
                .as_mut()
                .expect("available() > 0 implies a ring")
                .ring
                .read(len, |off, src| buf.write_at(off, src));
            let boost = pipe.rt_boost_pending;
            pipe.rt_boost_pending = false;
            (Some(n), boost)
        } else if pipe.writers == 0 {
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
    /// The first write to this pipe, and its ring page could not be
    /// allocated. Distinct from `None`: there is no amount of waiting that
    /// makes space appear, so a caller must not park on it.
    NoMemory,
}

pub fn try_write(pipe_id: PipeId, buf: &UserBytes) -> Option<PipeWrite> {
    with_pipes_mut(|pipes| {
        let pipe = pipes.get_mut(pipe_id)?;
        if pipe.readers == 0 {
            return Some(PipeWrite::BrokenPipe);
        }
        let Some(backing) = pipe.back() else {
            return Some(PipeWrite::NoMemory);
        };
        if backing.ring.space() > 0 {
            Some(PipeWrite::Wrote(backing.ring.write(buf.len(), |off, dst| buf.read_at(off, dst))))
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

/// Mark the pipe so the next consumer inherits RT priority. Called by the
/// wake path when the writer is RT (see `Pipe::rt_boost_pending`).
pub fn set_rt_boost_pending(pipe_id: PipeId) {
    with_pipes_mut(|pipes| {
        if let Some(pipe) = pipes.get_mut(pipe_id) {
            pipe.rt_boost_pending = true;
        }
    });
}

fn close_read(pipe_id: PipeId) {
    let wake_writers = with_pipes_mut(|pipes| {
        let pipe = pipes.get_mut(pipe_id).expect("close_read: pipe not found");
        pipe.readers = pipe.readers.checked_sub(1).expect("pipe reader underflow");
        pipe.publish_ends();
        if pipe.readers == 0 && pipe.writers == 0 {
            let pipe = pipes.remove(pipe_id).unwrap();
            free_pipe(pipe);
            None // pipe freed, no one to wake
        } else if pipe.readers == 0 {
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
                crate::io_uring::Source::PipeWritable(pipe_id),
            );
        }
    }
}

fn close_write(pipe_id: PipeId) {
    let wake_readers = with_pipes_mut(|pipes| {
        let pipe = pipes.get_mut(pipe_id).expect("close_write: pipe not found");
        pipe.writers = pipe.writers.checked_sub(1).expect("pipe writer underflow");
        pipe.publish_ends();
        if pipe.readers == 0 && pipe.writers == 0 {
            let pipe = pipes.remove(pipe_id).unwrap();
            free_pipe(pipe);
            None // pipe freed, no one to wake
        } else if pipe.writers == 0 {
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
                crate::io_uring::Source::PipeReadable(pipe_id),
            );
        }
    }
}

fn free_pipe(pipe: Pipe) {
    drop(pipe); // PhysPage freed via Drop
}

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

/// The waiter set of this pipe's read end, cloned out for a blocking site or a
/// wake path to hold on its own stack.
pub fn readers_queue(pipe_id: PipeId) -> Option<Arc<KWaitQueue>> {
    with_pipes(|pipes| pipes.get(pipe_id).map(|p| p.readers_wq.clone()))
}

pub fn writers_queue(pipe_id: PipeId) -> Option<Arc<KWaitQueue>> {
    with_pipes(|pipes| pipes.get(pipe_id).map(|p| p.writers_wq.clone()))
}

pub fn io_uring_watchers(pipe_id: PipeId) -> Vec<RingId> {
    with_pipes(|pipes| {
        pipes.get(pipe_id).map_or(Vec::new(), |p| p.io_uring_watchers.clone())
    })
}
