use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use hashbrown::HashMap;

use toyos_sched::task::WaitClass;

use crate::id_map::{IdKey, IdMap};
use crate::io_uring::RingId;
use crate::pipe::{PipeReader, PipeWriter};
use crate::process::Pid;
use crate::sched::payload::KWaitQueue;
use crate::sched::waitqs::new_queue;
use crate::sync::Lock;

// ListenerId — monotonic, never reused (same pattern as PipeId)

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct ListenerId(usize);

impl ListenerId {
    pub fn raw(self) -> usize { self.0 }
}

impl core::ops::Add for ListenerId {
    type Output = Self;
    fn add(self, rhs: Self) -> Self { ListenerId(self.0 + rhs.0) }
}

impl IdKey for ListenerId {
    const ZERO: Self = ListenerId(0);
    const ONE: Self = ListenerId(1);
}

/// A pending connection waiting for accept. Holds owned pipe references
/// that keep the pipes alive even if the client disconnects before accept.
pub struct PendingConnection {
    pub rx: PipeReader,
    pub tx: PipeWriter,
    pub client_pid: Pid,
}

struct Listener {
    /// The name this listener was registered under, so `remove` can unbind it
    /// while being addressed by id.
    name: String,
    /// The process that registered the name. A client's `connect` learns the
    /// server's pid from here — without it a client knows only a service name
    /// and could not name its own peer.
    owner: Pid,
    pending: VecDeque<PendingConnection>,
    io_uring_watchers: Vec<RingId>,
    /// Threads in `accept` on this listener (spec §8.6).
    acceptors: Arc<KWaitQueue>,
}

struct ListenerRegistry {
    by_id: IdMap<ListenerId, Listener>,
    by_name: HashMap<String, ListenerId>,
}

static LISTENERS: Lock<Option<ListenerRegistry>> = Lock::new(None);

pub fn init() {
    *LISTENERS.lock() = Some(ListenerRegistry {
        by_id: IdMap::new(),
        by_name: HashMap::new(),
    });
}

pub fn listen(name: &str, owner: Pid) -> Option<ListenerId> {
    let mut guard = LISTENERS.lock();
    let reg = guard.as_mut().unwrap();
    if reg.by_name.contains_key(name) {
        return None;
    }
    let id = reg.by_id.insert(Listener {
        name: String::from(name),
        owner,
        pending: VecDeque::new(),
        io_uring_watchers: Vec::new(),
        acceptors: new_queue(WaitClass::Ipc),
    });
    reg.by_name.insert(String::from(name), id);
    Some(id)
}

/// The process serving `name`.
pub fn owner(name: &str) -> Option<Pid> {
    let guard = LISTENERS.lock();
    let reg = guard.as_ref().unwrap();
    let &id = reg.by_name.get(name)?;
    reg.by_id.get(id).map(|l| l.owner)
}

/// Why a connection could not be queued.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PushError {
    /// The name stopped being served between the caller's `owner` lookup and
    /// here.
    NoListener,
    /// The server already holds `MAX_PENDING_CONNECTIONS` it has not accepted.
    QueueFull,
}

/// Connections one listener may hold unaccepted.
///
/// A burst allowance rather than a backlog: every server in the tree accepts
/// from its event loop, and the largest real burst is one connection per app
/// launch. Policy, like `MAX_FDS`.
///
/// It used to be the whole bound on the memory a connection flood could pin,
/// because each entry allocated two 2 MiB rings at `SYS_CONNECT`. It is not
/// that any more — a pipe allocates its page on first use, so an unaccepted
/// connection whose client has not written costs a `PendingConnection` and
/// nothing else. What this still bounds is the queue itself, which is the
/// unbounded collection the entry existed for.
///
/// It bounds one listener, not the machine: nothing caps how many listeners
/// exist.
pub const MAX_PENDING_CONNECTIONS: usize = 32;

pub fn push_connection(name: &str, conn: PendingConnection) -> Result<(), PushError> {
    let mut guard = LISTENERS.lock();
    let reg = guard.as_mut().unwrap();
    let Some(&id) = reg.by_name.get(name) else { return Err(PushError::NoListener) };
    let Some(listener) = reg.by_id.get_mut(id) else { return Err(PushError::NoListener) };
    if listener.pending.len() >= MAX_PENDING_CONNECTIONS {
        return Err(PushError::QueueFull);
    }
    listener.pending.push_back(conn);
    Ok(())
}

pub fn pop_connection(id: ListenerId) -> Option<PendingConnection> {
    let mut guard = LISTENERS.lock();
    let reg = guard.as_mut().unwrap();
    reg.by_id.get_mut(id)?.pending.pop_front()
}

pub fn has_pending_by_id(id: ListenerId) -> bool {
    let guard = LISTENERS.lock();
    let reg = guard.as_ref().unwrap();
    reg.by_id.get(id).map_or(false, |l| !l.pending.is_empty())
}

pub fn listener_id(name: &str) -> Option<ListenerId> {
    let guard = LISTENERS.lock();
    guard.as_ref().unwrap().by_name.get(name).copied()
}

/// Remove a listener. Pending connections are dropped (PipeReader/PipeWriter Drop frees pipes).
///
/// Addressed by id, never by name: `ListenerId`s come from an `IdMap` and are
/// never reused, so an id that has been removed names nothing forever. A
/// descriptor that outlived its listener therefore cannot unregister — or
/// accept on — whichever process holds that name now.
pub fn remove(id: ListenerId) {
    let mut guard = LISTENERS.lock();
    let reg = guard.as_mut().unwrap();
    if let Some(listener) = reg.by_id.remove(id) {
        reg.by_name.remove(&listener.name);
    }
}

pub fn add_io_uring_watcher(id: ListenerId, ring_id: RingId) {
    let mut guard = LISTENERS.lock();
    let reg = guard.as_mut().unwrap();
    if let Some(listener) = reg.by_id.get_mut(id) {
        if !listener.io_uring_watchers.contains(&ring_id) {
            listener.io_uring_watchers.push(ring_id);
        }
    }
}

pub fn remove_io_uring_watcher(id: ListenerId, ring_id: RingId) {
    let mut guard = LISTENERS.lock();
    let reg = guard.as_mut().unwrap();
    if let Some(listener) = reg.by_id.get_mut(id) {
        listener.io_uring_watchers.retain(|&x| x != ring_id);
    }
}

/// The waiter set of this listener, cloned out for a blocking `accept` or a
/// `connect`'s wake to hold on its own stack.
pub fn acceptors(id: ListenerId) -> Option<Arc<KWaitQueue>> {
    let guard = LISTENERS.lock();
    let reg = guard.as_ref().unwrap();
    reg.by_id.get(id).map(|l| l.acceptors.clone())
}

pub fn io_uring_watchers(id: ListenerId) -> Vec<RingId> {
    let guard = LISTENERS.lock();
    let reg = guard.as_ref().unwrap();
    reg.by_id.get(id).map_or(Vec::new(), |l| l.io_uring_watchers.clone())
}
