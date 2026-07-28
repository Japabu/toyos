//! Wait tickets — the shape of `toyos_sched::waitq` on top of the old
//! scheduler's blocked pool. Scheduler-core spec §8.1, migration stage 5.
//!
//! A blocking site registers *before* it re-checks its condition, and parks
//! with the registration in hand:
//!
//! ```text
//! let q = WaitQueue::pipe_readable(id);
//! let ticket = q.prepare_wait();
//! if q.ready() { ticket.cancel(); continue; }   // check-then-block gap: closed
//! scheduler::block_on(ticket, deadline);
//! ```
//!
//! Registrations live in the blocked pool, under the lock every wake path
//! already takes. A wake that lands while the waiter is still on its way to
//! that pool — the park window each source used to paper over with a recheck
//! of its own — finds the registration and marks it; the park then declines
//! and the caller loops. One mechanism for every source, futex included: a
//! mark needs no queryable readiness, which is the only reason the futex
//! wake-generation counter ever existed.
//!
//! What this is not: in the real protocol a waiter's home CPU serializes wake
//! against park by construction, so the window has no representation at all.
//! Here two paths merely share a lock, and the waiter still travels through a
//! global pool. The structural closure comes with the cutover (spec stage 7),
//! which deletes this module.

use core::marker::PhantomData;

use crate::pipe::PipeId;
use crate::scheduler::{self, EventSource, TaskId};
use crate::DirectMap;

/// The waiter set of one event source. Stage 7 gives every waitable object
/// its own queue; until then the set lives in the blocked pool and this is a
/// typed name for its key.
pub struct WaitQueue(EventSource);

impl WaitQueue {
    pub fn pipe_readable(id: PipeId) -> Self {
        Self(EventSource::PipeReadable(id))
    }

    pub fn pipe_writable(id: PipeId) -> Self {
        Self(EventSource::PipeWritable(id))
    }

    /// The waiters of one futex word, keyed by its physical address so the
    /// queue is shared across the processes that map it.
    pub fn futex(addr: DirectMap) -> Self {
        Self(EventSource::Futex(addr))
    }

    /// Phase 1: register the running thread. The caller must then re-check
    /// its condition and either cancel the ticket or block on it.
    ///
    /// Preemption is disabled for the ticket's lifetime. Not for the
    /// registration's integrity — the pool lock covers that — but because a
    /// thread preempted after deciding to block is no longer parked when its
    /// waker arrives, so it loses the priority the wake would have handed it:
    /// for the audio client that boost *is* its period deadline. The window
    /// ends in a reschedule either way, so nothing is owed to a preempt
    /// request raised inside it.
    #[must_use = "a wait ticket must be blocked on or cancelled"]
    pub fn prepare_wait(&self) -> WaitTicket<'_> {
        crate::preempt::disable();
        WaitTicket {
            queue: self,
            id: scheduler::register_wait(self.0),
            armed: true,
            _not_send: PhantomData,
        }
    }

    /// Is the source ready right now? The post-registration re-check.
    pub fn ready(&self) -> bool {
        source_ready(&self.0)
    }

    /// The whole two-phase commit, for a site whose re-check is exactly this
    /// queue's readiness: register, re-check, park. Returns when the thread
    /// runs again — spuriously in the general case, so callers loop.
    ///
    /// Sites that carry their own predicate (a futex word, a zombie table)
    /// spell the three steps out instead.
    pub fn wait(&self, deadline: u64) {
        let ticket = self.prepare_wait();
        if self.ready() {
            ticket.cancel();
        } else {
            scheduler::block_on(ticket, deadline);
        }
    }
}

/// Readiness of a source, re-derived from the source itself.
///
/// Futex reports false: a futex has no queryable state, and its waiters
/// re-check the user word instead. Every other caller of this is a waiter's
/// own re-check between registration and park.
pub fn source_ready(event: &EventSource) -> bool {
    match event {
        EventSource::Keyboard => crate::keyboard::has_data(),
        EventSource::Mouse => crate::mouse::has_data(),
        EventSource::Network => crate::net::has_packet(),
        EventSource::Listener(id) => crate::listener::has_pending_by_id(*id),
        EventSource::PipeReadable(id) => crate::pipe::has_data(*id),
        EventSource::PipeWritable(id) => crate::pipe::has_space(*id),
        EventSource::Audio => crate::audio::has_pending(),
        EventSource::Futex(_) => false,
        EventSource::IoUring(ring) => crate::io_uring::has_completions(*ring),
    }
}

/// A live registration on a queue, held by the thread that made it.
///
/// `!Send` — the registration belongs to the thread that owns it — and
/// drop-bombed: exactly one of [`WaitTicket::cancel`] and
/// [`scheduler::block_on`] may consume it, so "registered, then neither
/// parked nor withdrawn" cannot be reached by forgetting a branch.
#[must_use = "a wait ticket must be blocked on or cancelled"]
pub struct WaitTicket<'q> {
    queue: &'q WaitQueue,
    id: TaskId,
    armed: bool,
    _not_send: PhantomData<*mut ()>,
}

impl WaitTicket<'_> {
    /// The condition became true after registering: withdraw and keep running.
    ///
    /// A wake that already marked this ticket is discarded rather than
    /// deferred, and nothing is lost by that: every wake through the old pool
    /// is a broadcast to the source's whole waiter set, so no other waiter was
    /// deprived of it, and this caller cancels only with the condition in hand.
    pub fn cancel(mut self) {
        self.armed = false;
        scheduler::cancel_wait(self.id);
        crate::preempt::enable();
    }

    /// Phase 2, for `scheduler::block_on` only: consume the ticket and name
    /// the source the thread parks on. Preemption comes back without a poll —
    /// the caller's next act is the reschedule.
    pub(crate) fn into_park(mut self) -> EventSource {
        self.armed = false;
        crate::preempt::enable_no_resched();
        self.queue.0
    }
}

impl Drop for WaitTicket<'_> {
    fn drop(&mut self) {
        assert!(
            !self.armed,
            "wait ticket dropped: it must be blocked on or cancelled",
        );
    }
}
