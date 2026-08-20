//! The handle-facing object for an inbox.
//!
//! **Not the same `Inbox` as [`completion::inbox`](crate::completion::inbox).**
//! That one is a *task's* bounded record ring, minted at spawn and never named
//! by a handle. This one is what `SYS_INBOX_SETUP` installs: a process's
//! counted reference to the shared-memory submission/completion pair that
//! [`crate::inbox::Inbox`] owns. The two are unrelated today and share the
//! name because a later chunk means to converge them — see that module's own
//! header for the ordering claim this one rests on.
//!
//! Moved out of [`super::service`] with the 2026-08-20 rename: a connection
//! and an inbox are different KObjects, and `service.rs` is a connection's
//! file now.

use alloc::sync::Arc;

use crate::inbox::InboxRef;

use super::{Held, KObjectVariant, ObjectCore, ZeroHandles};

/// A process's counted reference to a submission/completion ring.
///
/// The ring's pages are the instance's, keyed by [`InboxId`]; this holds the
/// one counted reference to it.
///
/// [`InboxId`]: crate::inbox::InboxId
pub struct InboxObject {
    pub(super) core: ObjectCore,
    id: crate::inbox::InboxId,
    reference: Held<InboxRef>,
}

impl InboxObject {
    pub fn new(ring: InboxRef) -> Arc<Self> {
        Arc::new(Self {
            core: Self::new_core(),
            id: ring.id(),
            reference: Held::new(ring),
        })
    }

    pub fn id(&self) -> crate::inbox::InboxId {
        self.id
    }
}

impl ZeroHandles for InboxObject {
    fn on_zero_handles(&self) {
        self.reference.release();
    }
}
