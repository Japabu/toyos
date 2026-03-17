use alloc::collections::VecDeque;
use alloc::string::String;

use crate::pipe::{PipeReader, PipeWriter};
use crate::process::Pid;
use crate::sync::Lock;

/// A pending connection waiting for accept. Holds owned pipe references
/// that keep the pipes alive even if the client disconnects before accept.
pub struct PendingConnection {
    pub rx: PipeReader,
    pub tx: PipeWriter,
    pub client_pid: Pid,
}

struct Listener {
    pending: VecDeque<PendingConnection>,
}

static LISTENERS: Lock<Option<hashbrown::HashMap<String, Listener>>> = Lock::new(None);

pub fn init() {
    *LISTENERS.lock() = Some(hashbrown::HashMap::new());
}

pub fn listen(name: &str) -> bool {
    let mut guard = LISTENERS.lock();
    let map = guard.as_mut().unwrap();
    if map.contains_key(name) {
        return false;
    }
    map.insert(String::from(name), Listener { pending: VecDeque::new() });
    true
}

pub fn push_connection(name: &str, conn: PendingConnection) -> bool {
    let mut guard = LISTENERS.lock();
    let map = guard.as_mut().unwrap();
    if let Some(listener) = map.get_mut(name) {
        listener.pending.push_back(conn);
        true
    } else {
        false
    }
}

pub fn pop_connection(name: &str) -> Option<PendingConnection> {
    let mut guard = LISTENERS.lock();
    let map = guard.as_mut().unwrap();
    map.get_mut(name)?.pending.pop_front()
}

pub fn has_pending(name: &str) -> bool {
    let guard = LISTENERS.lock();
    guard.as_ref().unwrap().get(name).map_or(false, |l| !l.pending.is_empty())
}

pub fn exists(name: &str) -> bool {
    let guard = LISTENERS.lock();
    guard.as_ref().unwrap().contains_key(name)
}

/// Remove a listener. Pending connections are dropped (PipeReader/PipeWriter Drop frees pipes).
pub fn remove(name: &str) {
    let mut guard = LISTENERS.lock();
    guard.as_mut().unwrap().remove(name);
}
