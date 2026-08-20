//! A connection: two pipes for the bytes, two handle queues for the handles.
//!
//! The submission/completion ring's own object moved to [`super::inbox`] with
//! the 2026-08-20 rename — a different KObject, not a connection.

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;

use toyos_abi::syscall::{SyscallError, MAX_QUEUED_BATCHES};

use crate::pipe::{PipeId, PipeReader, PipeWriter};
use crate::sync::Lock;

use super::handle::HandleEntry;
use super::{Held, KObjectVariant, ObjectCore, ZeroHandles};

/// Handles in flight in one direction of a connection.
///
/// A batch is `HandleEntry`s, so `handle_count` stays raised for the whole
/// crossing: a shared-memory region sent to a client that dies before it
/// receives is released by this queue dropping, and nothing has to notice.
///
/// `None` once the end that would read this has gone: a sender then learns
/// `Gone` rather than filling a queue nobody will ever drain.
pub struct HandleQueue(Lock<Option<VecDeque<Vec<HandleEntry>>>>);

impl HandleQueue {
    fn open() -> Arc<Self> {
        Arc::new(Self(Lock::new(Some(VecDeque::new()))))
    }

    /// A queue with no reader and never one: what a connection joined out of
    /// two bare pipes has, because nothing holds its other half.
    fn dead() -> Arc<Self> {
        Arc::new(Self(Lock::new(None)))
    }

    /// **A refusal hands the batch back.** Both refusals below are ones a
    /// sender reads as backpressure, and a batch dropped here is capabilities
    /// destroyed under a word that says they were not — so the signature is
    /// what says the sender still owns them, and `HandleTable::transfer` is
    /// what puts each one back at its own number.
    fn push(&self, batch: Vec<HandleEntry>) -> Result<(), (Vec<HandleEntry>, SyscallError)> {
        let mut guard = self.0.lock();
        let Some(queue) = guard.as_mut() else { return Err((batch, SyscallError::Gone)) };
        if queue.len() >= MAX_QUEUED_BATCHES {
            return Err((batch, SyscallError::ResourceExhausted));
        }
        queue.push_back(batch);
        Ok(())
    }

    /// Take the oldest batch, or refuse without taking it when it is wider
    /// than `cap` — a receiver whose buffer is too small has to be told with
    /// the handles still queued, because nothing else can ask for them again.
    ///
    /// One acquisition for the size and the take, so the batch that was
    /// measured is the batch that comes back.
    fn pop_bounded(&self, cap: usize) -> Result<Option<Vec<HandleEntry>>, SyscallError> {
        let mut guard = self.0.lock();
        let Some(queue) = guard.as_mut() else { return Ok(None) };
        match queue.front() {
            None => Ok(None),
            Some(batch) if batch.len() > cap => Err(SyscallError::InvalidArgument),
            Some(_) => Ok(queue.pop_front()),
        }
    }

    /// How wide the oldest batch is, without taking it.
    ///
    /// `None` for an empty or closed queue. A caller measures with this and
    /// takes with [`pop_bounded`](Self::pop_bounded) rather than taking and
    /// then refusing: a batch dropped on a refusal is capabilities nobody can
    /// ask for again. Only the peer pushes, and only to the back, so the front
    /// this reports is the front the pop takes.
    fn front_width(&self) -> Option<usize> {
        self.0.lock().as_ref()?.front().map(Vec::len)
    }

    /// Nothing will ever read this again. Takes the batches out under the lock
    /// and drops them outside it, because releasing a handle can run another
    /// object's zero-handle hook.
    pub(super) fn close_now(&self) {
        let batches = self.0.lock().take();
        drop(batches);
    }
}

/// One end of an accepted or connected channel.
///
/// Two pipes for the bytes and two [`HandleQueue`]s for the handles, and the
/// queues are **cross-wired**: this end's `outbox` is the peer's `inbox`. So
/// "receive what I sent" is not a state that can be written, which is the same
/// property the two pipe ends already have.
pub struct ConnectionEnd {
    pub(super) core: ObjectCore,
    rx: PipeId,
    tx: PipeId,
    inbox: Arc<HandleQueue>,
    outbox: Arc<HandleQueue>,
    reference: Held<(PipeReader, PipeWriter)>,
}

impl ConnectionEnd {
    /// The two ends of a fresh connection, wired to each other.
    ///
    /// One call rather than two, because the cross-wiring is the invariant and
    /// a constructor per end would let a caller get it wrong. The server's end
    /// is built later, from what the pending connection carries.
    pub fn pair_queues() -> (Arc<HandleQueue>, Arc<HandleQueue>) {
        (HandleQueue::open(), HandleQueue::open())
    }

    pub fn new(
        rx: PipeReader,
        tx: PipeWriter,
        inbox: Arc<HandleQueue>,
        outbox: Arc<HandleQueue>,
    ) -> Arc<Self> {
        Arc::new(Self {
            core: Self::new_core(),
            rx: rx.id(),
            tx: tx.id(),
            inbox,
            outbox,
            reference: Held::new((rx, tx)),
        })
    }

    /// A duplex object over two pipe ends that were never a port's.
    ///
    /// `SYS_CONNECTION_JOIN`'s answer: netd's socket data path is two pipes and
    /// `std`'s `TcpStream` is one handle. Nothing holds the other half, so both
    /// queues are dead and handle transfer over it says so.
    pub fn joined(rx: PipeReader, tx: PipeWriter) -> Arc<Self> {
        Self::new(rx, tx, HandleQueue::dead(), HandleQueue::dead())
    }

    pub fn rx(&self) -> PipeId {
        self.rx
    }

    pub fn tx(&self) -> PipeId {
        self.tx
    }

    /// See [`HandleQueue::push`]: a refusal comes back with the batch.
    pub fn send(
        &self,
        batch: Vec<HandleEntry>,
    ) -> Result<(), (Vec<HandleEntry>, SyscallError)> {
        self.outbox.push(batch)
    }

    pub fn recv_bounded(&self, cap: usize) -> Result<Option<Vec<HandleEntry>>, SyscallError> {
        self.inbox.pop_bounded(cap)
    }

    /// See [`HandleQueue::front_width`].
    pub fn peek_width(&self) -> Option<usize> {
        self.inbox.front_width()
    }
}

impl ZeroHandles for ConnectionEnd {
    fn on_zero_handles(&self) {
        // The inbox and not the outbox: what the peer has already sent is
        // this end's to release, and what this end sent is still the peer's to
        // receive — the same rule as bytes already in the pipe.
        self.inbox.close_now();
        self.reference.release();
    }
}
