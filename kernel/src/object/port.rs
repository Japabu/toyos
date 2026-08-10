//! A port: the thing a service *is*, once it stops being a name.
//!
//! Two object types over one shared queue. A server holds the [`Acceptor`] and
//! a client holds a [`Connector`], so "accept the connections of a service you
//! were only given access to" is a state that cannot be written rather than a
//! runtime `PermissionDenied` — the same reason a pipe's two ends are two
//! types.
//!
//! **Both ends exist before either process runs**, which is the whole
//! mechanism: `/bin/init` creates the port and endows the two halves, so a
//! client's first instruction can connect whether or not the server has
//! reached `accept` or has even been spawned. There is no instant at which a
//! name is not bound yet, so there is nothing to retry and no timeout
//! anywhere.

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;

use toyos_sched::task::WaitClass;

use crate::io_uring::RingId;
use crate::pipe::{PipeReader, PipeWriter};
use crate::sched::payload::KWaitQueue;
use crate::sched::waitqs::{new_queue, wake_all};
use crate::sync::Lock;

use super::service::HandleQueue;
use super::{KObjectVariant, ObjectCore, ZeroHandles};

/// Unaccepted connections one port may hold.
///
/// Policy on the primitive: past it a client sees `ResourceExhausted`, and a
/// queued connection costs a `PendingConnection` and no memory at all until
/// somebody writes a byte on it.
pub const MAX_PENDING_CONNECTIONS: usize = 32;

/// A connection nobody has accepted yet.
///
/// It owns the server's two pipe ends and its two handle queues, so a client
/// that exits before the accept leaves the server something to read an EOF from
/// rather than nothing — and a client that sent handles before the accept
/// leaves them where the accept will find them.
pub struct PendingConnection {
    pub rx: PipeReader,
    pub tx: PipeWriter,
    pub inbox: Arc<HandleQueue>,
    pub outbox: Arc<HandleQueue>,
}

/// The queue, and whether anything will ever read it again.
///
/// **One lock over both, because `closed` is a decision *about* the queue.** It
/// was an `AtomicBool` read outside the lock and taken afterwards, which buys a
/// lock-free read on a path that takes the lock in the next statement anyway —
/// and leaves a window: a connect that reads `closed == false`, an
/// [`Acceptor`]'s hook that then sets it and drains the queue, and a
/// `push_back` that lands in a queue nothing will ever look at again. The hook
/// runs exactly once, so that connection is orphaned: nothing closes its inbox,
/// the client's write succeeds into a ring nobody reads and its read blocks for
/// ever. That is the one outcome this design says cannot happen — the bound on
/// failure is a process lifetime — and there it is no bound at all.
struct PortQueue {
    closed: bool,
    pending: VecDeque<PendingConnection>,
}

/// Everything the two ends share. Neither end holds the other, so no `Arc`
/// cycle exists.
pub struct PortShared {
    queue: Lock<PortQueue>,
    /// Threads blocked in `accept`.
    acceptors: Arc<KWaitQueue>,
    io_uring_watchers: Lock<Vec<RingId>>,
}

pub struct Acceptor {
    pub(super) core: ObjectCore,
    shared: Arc<PortShared>,
}

pub struct Connector {
    pub(super) core: ObjectCore,
    shared: Arc<PortShared>,
}

/// Why a connection was not queued.
pub enum PushError {
    /// The acceptor is gone: the server exited, or never existed.
    Closed,
    QueueFull,
}

pub fn create() -> (Arc<Acceptor>, Arc<Connector>) {
    let shared = Arc::new(PortShared {
        queue: Lock::new(PortQueue { closed: false, pending: VecDeque::new() }),
        acceptors: new_queue(WaitClass::Ipc),
        io_uring_watchers: Lock::new(Vec::new()),
    });
    (
        Arc::new(Acceptor { core: Acceptor::new_core(), shared: shared.clone() }),
        Arc::new(Connector { core: Connector::new_core(), shared }),
    )
}

/// **The io_uring watch names the port, not either end**, because a client
/// connecting through a `Connector` has to complete a poll a server registered
/// on the `Acceptor` — and the two share exactly this.
impl PortShared {
    pub fn has_pending(&self) -> bool {
        !self.queue.lock().pending.is_empty()
    }

    fn closed(&self) -> bool {
        self.queue.lock().closed
    }

    /// The waiter set, cloned out so a blocking site can hold it across its own
    /// park — the ticket borrows the queue, not the port.
    pub fn waiters(&self) -> Arc<KWaitQueue> {
        self.acceptors.clone()
    }

    pub fn watchers(&self) -> Vec<RingId> {
        self.io_uring_watchers.lock().clone()
    }

    pub fn add_watcher(&self, ring: RingId) {
        let mut watchers = self.io_uring_watchers.lock();
        if !watchers.contains(&ring) {
            watchers.push(ring);
        }
    }

    pub fn remove_watcher(&self, ring: RingId) {
        self.io_uring_watchers.lock().retain(|&id| id != ring);
    }
}

impl Acceptor {
    pub fn pop(&self) -> Option<PendingConnection> {
        self.shared.queue.lock().pending.pop_front()
    }

    /// The last handle to this acceptor has gone: nothing will ever be queued
    /// again, so a thread parked in `accept` has to leave rather than wait for
    /// a condition that has become permanently false.
    pub fn closed(&self) -> bool {
        self.shared.closed()
    }

    pub fn has_pending(&self) -> bool {
        self.shared.has_pending()
    }

    pub fn waiters(&self) -> Arc<KWaitQueue> {
        self.shared.waiters()
    }

    pub fn port(&self) -> Arc<PortShared> {
        self.shared.clone()
    }
}

impl Connector {
    pub fn closed(&self) -> bool {
        self.shared.closed()
    }

    /// **One acquisition for the question and the insert.** See [`PortQueue`].
    pub fn push(&self, connection: PendingConnection) -> Result<(), PushError> {
        let mut queue = self.shared.queue.lock();
        if queue.closed {
            return Err(PushError::Closed);
        }
        if queue.pending.len() >= MAX_PENDING_CONNECTIONS {
            return Err(PushError::QueueFull);
        }
        queue.pending.push_back(connection);
        Ok(())
    }

    pub fn waiters(&self) -> Arc<KWaitQueue> {
        self.shared.waiters()
    }

    pub fn port(&self) -> Arc<PortShared> {
        self.shared.clone()
    }
}

/// The port closes and every queued connection's pipe ends drop, which is what
/// makes each waiting client's next write `Gone` and its next read `0`. A
/// server that exited without serving is a bound of one process lifetime and
/// nothing else.
///
/// **Every thread parked in `accept` is woken too.** They are parked on a
/// condition — "the queue has something" — that has just become permanently
/// false, and a wake is the only thing that lets them re-read `closed` and
/// leave.
impl ZeroHandles for Acceptor {
    fn on_zero_handles(&self) {
        // Closing and draining in one acquisition, so no connect can be between
        // the two. The batches are dropped after the guard: releasing a handle
        // can run another object's zero-handle hook.
        let queued = {
            let mut queue = self.shared.queue.lock();
            queue.closed = true;
            core::mem::take(&mut queue.pending)
        };
        // The would-be server's inbox: nobody will ever hold the end that
        // reads it, so a client's `SYS_HANDLE_SEND` on that connection must
        // say `Gone` rather than queue.
        for connection in &queued {
            connection.inbox.close_now();
        }
        drop(queued);
        wake_all(&self.shared.acceptors);
    }
}
