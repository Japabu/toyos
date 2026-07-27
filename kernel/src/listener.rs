use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;
use hashbrown::HashMap;

use crate::id_map::{IdKey, IdMap};
use crate::io_uring::RingId;
use crate::pipe::{PipeReader, PipeWriter};
use crate::process::Pid;
use crate::sync::Lock;

// ---------------------------------------------------------------------------
// ListenerId — monotonic, never reused (same pattern as PipeId)
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// PendingConnection
// ---------------------------------------------------------------------------

/// A pending connection waiting for accept. Holds owned pipe references
/// that keep the pipes alive even if the client disconnects before accept.
pub struct PendingConnection {
    pub rx: PipeReader,
    pub tx: PipeWriter,
    pub client_pid: Pid,
}

// ---------------------------------------------------------------------------
// Listener + registry
// ---------------------------------------------------------------------------

struct Listener {
    pending: VecDeque<PendingConnection>,
    io_uring_watchers: Vec<RingId>,
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

pub fn listen(name: &str) -> Option<ListenerId> {
    let mut guard = LISTENERS.lock();
    let reg = guard.as_mut().unwrap();
    if reg.by_name.contains_key(name) {
        return None;
    }
    let id = reg.by_id.insert(Listener {
        pending: VecDeque::new(),
        io_uring_watchers: Vec::new(),
    });
    reg.by_name.insert(String::from(name), id);
    Some(id)
}

pub fn push_connection(name: &str, conn: PendingConnection) -> bool {
    let mut guard = LISTENERS.lock();
    let reg = guard.as_mut().unwrap();
    let Some(&id) = reg.by_name.get(name) else { return false };
    let Some(listener) = reg.by_id.get_mut(id) else { return false };
    listener.pending.push_back(conn);
    true
}

pub fn pop_connection(name: &str) -> Option<PendingConnection> {
    let mut guard = LISTENERS.lock();
    let reg = guard.as_mut().unwrap();
    let &id = reg.by_name.get(name)?;
    reg.by_id.get_mut(id)?.pending.pop_front()
}

pub fn has_pending(name: &str) -> bool {
    let guard = LISTENERS.lock();
    let reg = guard.as_ref().unwrap();
    let Some(&id) = reg.by_name.get(name) else { return false };
    reg.by_id.get(id).map_or(false, |l| !l.pending.is_empty())
}

pub fn has_pending_by_id(id: ListenerId) -> bool {
    let guard = LISTENERS.lock();
    let reg = guard.as_ref().unwrap();
    reg.by_id.get(id).map_or(false, |l| !l.pending.is_empty())
}

pub fn exists(name: &str) -> bool {
    let guard = LISTENERS.lock();
    guard.as_ref().unwrap().by_name.contains_key(name)
}

pub fn listener_id(name: &str) -> Option<ListenerId> {
    let guard = LISTENERS.lock();
    guard.as_ref().unwrap().by_name.get(name).copied()
}


/// Remove a listener. Pending connections are dropped (PipeReader/PipeWriter Drop frees pipes).
pub fn remove(name: &str) {
    let mut guard = LISTENERS.lock();
    let reg = guard.as_mut().unwrap();
    if let Some(id) = reg.by_name.remove(name) {
        reg.by_id.remove(id);
    }
}

// ---------------------------------------------------------------------------
// Per-listener io_uring watchers
// ---------------------------------------------------------------------------

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

pub fn io_uring_watchers(id: ListenerId) -> Vec<RingId> {
    let guard = LISTENERS.lock();
    let reg = guard.as_ref().unwrap();
    reg.by_id.get(id).map_or(Vec::new(), |l| l.io_uring_watchers.clone())
}
