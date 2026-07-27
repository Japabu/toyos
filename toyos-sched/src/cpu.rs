//! The per-CPU machine. `CpuSched`, the `SchedPass` type-state and `Action`
//! land at spec §6 (migration Stage 4). What exists today: the two tokens the
//! [`crate::hw::Hw`] boundary consumes, and the globally reachable per-CPU
//! **handle** — the only thing remote code can touch, and all it can do
//! through it is post a message and ring the doorbell (spec §6.1).

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::hw::CpuId;
use crate::mailbox::{
    Doorbell, Kick, MailboxProducer, PostSlot, PreemptGuard, SchedMsg, SleepArm, Urgency,
};
use crate::sync::{AtomicU32, Ordering};
use crate::task::SchedPayload;

/// Permission to switch to a picked task. Holds pointers into the stable
/// Box-backed task records (spec §5.1); constructed only by safe code in
/// `SchedPass::finish`, consumed by the driver's `unsafe Hw::switch`.
#[must_use]
pub struct RunToken<X: SchedPayload> {
    restore: *const X::Ctx,
    save: *mut X::Ctx,
}

impl<X: SchedPayload> RunToken<X> {
    /// The incoming task's saved context to restore.
    pub fn restore_ptr(&self) -> *const X::Ctx {
        self.restore
    }

    /// Where the outgoing context must be saved.
    pub fn save_ptr(&self) -> *mut X::Ctx {
        self.save
    }
}

/// Proof that halting is safe. The only constructor takes a confirmed
/// [`SleepArm`] (spec §7.5): SLEEPING was published before the final
/// mailbox-empty check, so any message that check missed rings the doorbell
/// afterwards and its producer sends the IPI. Stage 4's `finish()` adds the
/// remaining preconditions — empty run queue, timer programmed for the
/// deadline-heap minimum — by being the only caller.
#[must_use]
pub struct SleepToken {
    _private: (),
}

impl SleepToken {
    pub(crate) fn new(_confirmed: SleepArm<'_>) -> Self {
        Self { _private: () }
    }
}

/// The globally shared, `Sync` face of a CPU. There is no global array of
/// `CpuSched` — a `static` of a `!Sync` type does not compile — so this is
/// the whole remote surface: post a message, ring the doorbell, read the
/// published load (spec §6.1).
pub struct CpuHandle<M> {
    id: CpuId,
    post: MailboxProducer<M>,
    doorbell: Doorbell,
    /// Ready-count heuristic, published for spawn placement only.
    load: AtomicU32,
}

impl<M: SchedMsg> CpuHandle<M> {
    pub fn new(id: CpuId, post: MailboxProducer<M>) -> Self {
        Self {
            id,
            post,
            doorbell: Doorbell::new(),
            load: AtomicU32::new(0),
        }
    }

    pub fn id(&self) -> CpuId {
        self.id
    }

    pub fn doorbell(&self) -> &Doorbell {
        &self.doorbell
    }

    pub fn load(&self) -> u32 {
        self.load.load(Ordering::Relaxed)
    }

    pub fn publish_load(&self, ready: u32) {
        self.load.store(ready, Ordering::Relaxed);
    }

    /// Post one message and ring the doorbell. The returned [`Kick`] is the
    /// caller's obligation: `Kick::Send` means the targeted IPI must go out
    /// (spec §7.3).
    pub fn post(
        &self,
        slot: PostSlot<'_, M>,
        msg: M,
        urgency: Urgency,
        preempt: &impl PreemptGuard,
    ) -> Kick {
        self.post.post(slot, msg, preempt);
        self.doorbell.ring(urgency)
    }
}

/// The boot-initialized slice of handles. Indexed by [`CpuId`]; an unknown
/// CPU id is a bug, not a lookup failure.
pub struct CpuHandles<M> {
    handles: Box<[CpuHandle<M>]>,
}

impl<M: SchedMsg> CpuHandles<M> {
    pub fn new(handles: Vec<CpuHandle<M>>) -> Self {
        for (index, handle) in handles.iter().enumerate() {
            assert_eq!(
                handle.id(),
                CpuId(index as u32),
                "cpu handles must be indexed by their own id",
            );
        }
        Self {
            handles: handles.into_boxed_slice(),
        }
    }

    pub fn get(&self, cpu: CpuId) -> &CpuHandle<M> {
        self.handles
            .get(cpu.0 as usize)
            .unwrap_or_else(|| panic!("no such cpu: {cpu:?}"))
    }

    pub fn len(&self) -> usize {
        self.handles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.handles.is_empty()
    }
}
