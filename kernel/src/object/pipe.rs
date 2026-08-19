//! A pipe's two ends, as two object types.
//!
//! "Write to a read end" is a state that cannot be written rather than a
//! runtime `PermissionDenied`: [`PipeReadEnd`] has no write path, and the typed
//! accessor refuses the handle before any policy runs.
//!
//! **The ring, its refcounts and its `PipeId` stay in `crate::pipe` until
//! chunk 6.** `SYS_PIPE_OPEN`, `SYS_PIPE_ID` and `SYS_SOCKET_CREATE` are live
//! until then and every one of them is addressed by that id, so a `PipeShared`
//! here would be a second owner of the same counts. What these two types own is
//! the *end* — one counted reference each — and giving it back on the last
//! handle is what makes EOF ride `handle_count`.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::pipe::{PipeId, PipeReader, PipeWriter};

use super::{Held, KObjectVariant, ObjectCore, ZeroHandles};

pub struct PipeReadEnd {
    pub(super) core: ObjectCore,
    /// A plain copy, because every read reaches for it and an `Arc` clone or a
    /// second lock on that path is an atomic read-modify-write TCG cannot emit
    /// inline
    /// (`issues/hardware/one-rmw-per-log-line-cost-350ms.md`). It names
    /// nothing once the reference below is given back: `IdMap` never reissues
    /// a `PipeId`.
    id: PipeId,
    tty: AtomicBool,
    reference: Held<PipeReader>,
}

pub struct PipeWriteEnd {
    pub(super) core: ObjectCore,
    id: PipeId,
    tty: AtomicBool,
    reference: Held<PipeWriter>,
}

impl PipeReadEnd {
    /// A second counted reference to the same pipe, for a caller building
    /// another object over it — `SYS_CONNECTION_JOIN` is the one.
    pub fn reference(&self) -> PipeReader {
        self.reference.get().expect("a live handle names this end")
    }

    pub fn new(reader: PipeReader) -> Arc<Self> {
        Arc::new(Self {
            core: Self::new_core(),
            id: reader.id(),
            tty: AtomicBool::new(false),
            reference: Held::new(reader),
        })
    }

    pub fn id(&self) -> PipeId {
        self.id
    }

    pub fn is_tty(&self) -> bool {
        self.tty.load(Ordering::Relaxed)
    }

    /// A mark on this end, not on the pipe.
    ///
    /// `SYS_MARK_TTY` converts one handle and its one caller marks both ends of
    /// a pair separately (std's `Command`), so a flag on the shared ring would
    /// be a wider claim than anything ever makes.
    pub fn mark_tty(&self) {
        self.tty.store(true, Ordering::Relaxed);
    }
}

impl PipeWriteEnd {
    pub fn reference(&self) -> PipeWriter {
        self.reference.get().expect("a live handle names this end")
    }

    pub fn new(writer: PipeWriter) -> Arc<Self> {
        Arc::new(Self {
            core: Self::new_core(),
            id: writer.id(),
            tty: AtomicBool::new(false),
            reference: Held::new(writer),
        })
    }

    pub fn id(&self) -> PipeId {
        self.id
    }

    pub fn is_tty(&self) -> bool {
        self.tty.load(Ordering::Relaxed)
    }

    pub fn mark_tty(&self) {
        self.tty.store(true, Ordering::Relaxed);
    }
}

impl ZeroHandles for PipeReadEnd {
    fn on_zero_handles(&self) {
        self.reference.release();
    }
}

impl ZeroHandles for PipeWriteEnd {
    fn on_zero_handles(&self) {
        self.reference.release();
    }
}
