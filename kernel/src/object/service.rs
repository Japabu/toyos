//! A connection, and the registered service it was accepted from.
//!
//! [`ListenerObject`] is transitional and chunk 3 deletes it: a listener that
//! holds a *name* is the object the endowment architecture exists to remove,
//! and `Acceptor`/`Connector` replace it. It is here so that chunk 2 — which is
//! the `Descriptor` → `KObjectRef` move and nothing else — stays green.

use alloc::sync::Arc;

use crate::io_uring::RingRef;
use crate::listener::{ListenerId, ListenerRef};
use crate::pipe::{PipeId, PipeReader, PipeWriter};
use crate::process::Pid;

use super::{Held, KObjectVariant, ObjectCore, ZeroHandles};

/// One end of an accepted or connected channel.
///
/// `peer` survives until chunk 6: the compositor and soundd both grant shared
/// memory to the pid `accept` reports and have nothing else to grant to until
/// handle transfer exists (`specs/capability-endowment-spec.md` §3.3).
pub struct ConnectionEnd {
    pub(super) core: ObjectCore,
    rx: PipeId,
    tx: PipeId,
    peer: Pid,
    reference: Held<(PipeReader, PipeWriter)>,
}

impl ConnectionEnd {
    pub fn new(rx: PipeReader, tx: PipeWriter, peer: Pid) -> Arc<Self> {
        Arc::new(Self {
            core: Self::new_core(),
            rx: rx.id(),
            tx: tx.id(),
            peer,
            reference: Held::new((rx, tx)),
        })
    }

    pub fn rx(&self) -> PipeId {
        self.rx
    }

    pub fn tx(&self) -> PipeId {
        self.tx
    }

    pub fn peer(&self) -> Pid {
        self.peer
    }
}

impl ZeroHandles for ConnectionEnd {
    fn on_zero_handles(&self) {
        self.reference.release();
    }
}

pub struct ListenerObject {
    pub(super) core: ObjectCore,
    id: ListenerId,
    reference: Held<ListenerRef>,
}

impl ListenerObject {
    pub fn new(listener: ListenerRef) -> Arc<Self> {
        Arc::new(Self {
            core: Self::new_core(),
            id: listener.id(),
            reference: Held::new(listener),
        })
    }

    pub fn id(&self) -> ListenerId {
        self.id
    }
}

impl ZeroHandles for ListenerObject {
    fn on_zero_handles(&self) {
        self.reference.release();
    }
}

/// A submission/completion ring.
///
/// The ring's pages are the instance's, keyed by [`RingId`]; this holds the one
/// counted reference to it. Chunk 6 moves the `PageAlloc` in here and deletes
/// the `SharedToken` the setup call still hands back.
pub struct IoUringObject {
    pub(super) core: ObjectCore,
    id: crate::io_uring::RingId,
    reference: Held<RingRef>,
}

impl IoUringObject {
    pub fn new(ring: RingRef) -> Arc<Self> {
        Arc::new(Self {
            core: Self::new_core(),
            id: ring.id(),
            reference: Held::new(ring),
        })
    }

    pub fn id(&self) -> crate::io_uring::RingId {
        self.id
    }
}

impl ZeroHandles for IoUringObject {
    fn on_zero_handles(&self) {
        self.reference.release();
    }
}
