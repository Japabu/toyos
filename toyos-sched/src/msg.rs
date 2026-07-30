//! The cross-CPU message set — spec §7.1.
//!
//! Two of the four messages are requests about a task the target already
//! owns; one *is* the ownership transfer. That is the property that makes
//! overflow unrepresentable (§7.2): a queue that cannot drop a message cannot
//! lose a task, because the message is the task.
//!
//! Its own module rather than part of `mailbox.rs`, so that the crate's single
//! `#[allow(unsafe_code)]` island stays exactly the intrusive-list code.

use crate::hw::CpuId;
use crate::mailbox::{MailboxNode, SchedMsg};
use crate::sync::Arc;
use crate::task::{SchedPayload, TaskKey, TaskShared, TransitTask, WakeCause};

pub enum Msg<X: SchedPayload> {
    /// The target CPU owns the parked task; this is a request, not a
    /// transfer. Rides on `TaskShared.wake_node`.
    Wake { key: TaskKey, cause: WakeCause },
    /// Ownership transfer: spawn placement, migration, wake-forwarding. Rides
    /// on the node embedded in the transferred task's own record, which is
    /// why a transfer can never be dropped for lack of queue space.
    Adopt { task: TransitTask<X> },
    /// "If you are overloaded, send me one" (spec §7.7). Rides on the thief's
    /// single reusable probe node.
    StealRequest { thief: CpuId },
    /// Kill protocol (spec §7.6). Rides on `TaskShared.retire_node`.
    Retire { shared: Arc<TaskShared<Msg<X>>> },
}

impl<X: SchedPayload> Msg<X> {
    /// Where an `Adopt`'s node lives: inside the task record the message
    /// carries. Handed to `MailboxProducer::post_owned`, whose safety
    /// condition is exactly what `Task`'s `Box` guarantees — the node's
    /// address does not change when the message moves.
    pub fn adopt_node(&self) -> &MailboxNode<Msg<X>> {
        match self {
            Msg::Adopt { task } => task.adopt_node(),
            _ => panic!("adopt_node on a message that does not carry a task"),
        }
    }
}

impl<X: SchedPayload> SchedMsg for Msg<X> {
    fn wake(key: TaskKey, cause: WakeCause) -> Self {
        Msg::Wake { key, cause }
    }

    /// The spec's `Retire { key, notify: TaskRef }` field is deliberately
    /// absent: a notify riding the message would be a second wake path (what
    /// §8.2 exists to prevent) and would have to outlive the message anyway,
    /// since a *running* target consumes the retire and dies at some later
    /// safe point. Joiners wait on the environment's finalize sink instead,
    /// which runs exactly once per task and after the payload is gone.
    fn retire(shared: Arc<TaskShared<Msg<X>>>) -> Self {
        Msg::Retire { shared }
    }
}

impl<X: SchedPayload> core::fmt::Debug for Msg<X> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Msg::Wake { key, cause } => write!(f, "Wake({:?}, {:?})", key, cause.reason),
            Msg::Adopt { task } => write!(f, "Adopt({:?})", task.key()),
            Msg::StealRequest { thief } => write!(f, "StealRequest({thief:?})"),
            Msg::Retire { shared } => write!(f, "Retire({:?})", shared.key()),
        }
    }
}
