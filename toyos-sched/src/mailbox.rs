//! Intrusive MPSC mailbox and doorbell — spec §7.2, §7.3, §7.5.
//!
//! One queue per CPU. Producers are any CPU or IRQ context; the consumer is
//! the owning CPU, at pass start. Nodes are **embedded in the objects the
//! messages are about** (`TaskShared.wake_node`, `TaskShared.retire_node`,
//! the task record's adopt node, the per-CPU steal probe) and never
//! allocated, so there is no capacity to size wrong: **queue overflow has no
//! representation** (B8), and an ownership-carrying message cannot be dropped
//! because the message *is* the owner.
//!
//! This is the only module in the crate allowed to write `unsafe`
//! (spec §4). Everything it does rests on three invariants, each restated at
//! its use site:
//!
//! * **N1 (single claim).** A node carries at most one message at a time —
//!   [`MailboxNode::claim`] is the only way to post and it hands out an
//!   exclusive [`PostSlot`] until the consumer has fully unlinked the node.
//!   This is invariant I12, enforced here rather than argued at the call
//!   sites.
//! * **N2 (liveness).** A node's owner outlives the message: the kernel's
//!   nodes live in the `Arc<TaskShared>` / boxed task record the message is
//!   about, and the retire protocol keeps them alive until the home CPU has
//!   consumed the message. Dropping a node while a message from it is queued
//!   is caught by [`MailboxNode`]'s drop bomb — loudly, before the memory is
//!   released, in the fail-fast spirit of the rest of the crate.
//! * **N3 (preempt-disabled push).** Every producer pushes inside a
//!   preempt-disabled region: [`MailboxProducer::post`] demands a
//!   [`PreemptGuard`], so a caller *cannot* type an unguarded push. See the
//!   torn-push discussion on [`MailboxProducer::post`].

#![allow(unsafe_code)]

use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::ptr;

use crate::sync::{Arc, AtomicBool, AtomicPtr, AtomicU32, Ordering};
use crate::task::{TaskKey, TaskShared, WakeCause};

/// The message vocabulary the primitives need to speak.
///
/// The full message set of spec §7.1 — including the ownership-carrying
/// `Adopt { task: TransitTask<X> }`, which only exists once tasks are linear
/// values — is defined by the environment at Stage 4. Wait queues and the
/// retire protocol construct only these two, so they stay free of the task
/// payload type.
pub trait SchedMsg: Send + Sized {
    fn wake(key: TaskKey, cause: WakeCause) -> Self;

    /// A retire carries the whole [`TaskShared`], not just the key: the home
    /// CPU that finds the task gone must read the state word to chase it
    /// (spec §7.6), and a bare key gives it nothing to read.
    fn retire(shared: Arc<TaskShared<Self>>) -> Self;
}

/// Proof that preemption is disabled for as long as the guard is borrowed.
///
/// # Safety
/// An implementor must guarantee that the executing context cannot be
/// descheduled while a value of the type is alive: the kernel's preempt-count
/// guard, an IRQ context (which cannot be preempted at all), or a model that
/// represents either. Implementing this on a type that does not disable
/// preemption reintroduces the stranded-suffix failure (spec §12, and the
/// negative loom case `preempted_producer_strands_suffix`).
pub unsafe trait PreemptGuard {}

#[cfg(not(feature = "loom"))]
mod slot {
    /// One message's storage inside an embedded node. Interior mutability is
    /// required because producers reach the node through a shared reference;
    /// the accesses are ordered by the queue's release/acquire pair, never
    /// concurrent (see N1 and the `Sync` justification in the parent module).
    pub struct Slot<M>(core::cell::UnsafeCell<Option<M>>);

    impl<M> Slot<M> {
        pub fn new() -> Self {
            Self(core::cell::UnsafeCell::new(None))
        }

        /// # Safety
        /// The caller holds the node's single claim (N1) and has not yet
        /// published the node, so no consumer can observe the slot.
        pub unsafe fn put(&self, msg: M) {
            unsafe { *self.0.get() = Some(msg) };
        }

        /// # Safety
        /// The caller is the consumer and has unlinked the node, so no
        /// producer can observe the slot.
        pub unsafe fn take(&self) -> Option<M> {
            unsafe { (*self.0.get()).take() }
        }
    }
}

#[cfg(feature = "loom")]
mod slot {
    pub struct Slot<M>(loom::cell::UnsafeCell<Option<M>>);

    impl<M> Slot<M> {
        pub fn new() -> Self {
            Self(loom::cell::UnsafeCell::new(None))
        }

        /// # Safety
        /// See the non-loom twin: the claim (N1) makes this exclusive. Loom
        /// checks the claim empirically — a concurrent slot access aborts the
        /// model.
        pub unsafe fn put(&self, msg: M) {
            self.0.with_mut(|p| unsafe { *p = Some(msg) });
        }

        /// # Safety
        /// See the non-loom twin.
        pub unsafe fn take(&self) -> Option<M> {
            self.0.with_mut(|p| unsafe { (*p).take() })
        }
    }
}

use slot::Slot;

/// The embedded link a message rides on. Never allocated, never counted.
pub struct MailboxNode<M> {
    next: AtomicPtr<MailboxNode<M>>,
    slot: Slot<M>,
    /// N1: set by [`Self::claim`], cleared by the consumer *after* the node
    /// is unlinked. Also the §7.7 steal-probe recycling flag — one mechanism
    /// for every node kind instead of a special case for one of them.
    in_flight: AtomicBool,
}

// SAFETY: `slot` is written by the producer strictly before the node is
// published (the Release store into the predecessor's `next`) and read by the
// consumer strictly after it observes that store (Acquire) and unlinks the
// node. The two accesses are ordered and never overlap (N1), so sharing the
// node across CPUs is sound whenever the message itself may cross CPUs.
unsafe impl<M: Send> Sync for MailboxNode<M> {}
// SAFETY: as above — the node is inert unless claimed.
unsafe impl<M: Send> Send for MailboxNode<M> {}

impl<M> MailboxNode<M> {
    pub fn new() -> Self {
        Self {
            next: AtomicPtr::new(ptr::null_mut()),
            slot: Slot::new(),
            in_flight: AtomicBool::new(false),
        }
    }

    /// Claim the exclusive right to post this node (N1 / invariant I12).
    ///
    /// `None` means a message from this node is still in flight. The steal
    /// probe treats that as "a probe is already outstanding, don't post
    /// another" (spec §7.7); the wake and retire nodes cannot legitimately
    /// see it — their higher-level CAS (`Blocked → WakeQueued`) and sticky
    /// `RETIRE_QUEUED` bit admit exactly one poster — so those call sites
    /// unwrap and fail fast.
    pub fn claim(&self) -> Option<PostSlot<'_, M>> {
        if self.in_flight.swap(true, Ordering::AcqRel) {
            None
        } else {
            Some(PostSlot { node: self })
        }
    }

    pub fn in_flight(&self) -> bool {
        self.in_flight.load(Ordering::Acquire)
    }
}

impl<M> Default for MailboxNode<M> {
    fn default() -> Self {
        Self::new()
    }
}

impl<M> Drop for MailboxNode<M> {
    /// N2's detector: freeing a node whose message is still queued would
    /// leave the consumer walking freed memory. Safe Rust cannot release the
    /// storage without running this, so the bug becomes a loud panic at the
    /// exact site instead of a silent use-after-free later.
    fn drop(&mut self) {
        assert!(
            !self.in_flight.load(Ordering::Relaxed),
            "mailbox node dropped with a message in flight",
        );
    }
}

/// The exclusive right to post one message on a node. Consumed by
/// [`MailboxProducer::post`]; dropping it instead releases the claim, so a
/// forgotten post cannot wedge the node.
#[must_use = "a claimed node must be posted or released"]
pub struct PostSlot<'n, M> {
    node: &'n MailboxNode<M>,
}

impl<M> PostSlot<'_, M> {
    /// Give the claim back without posting.
    pub fn release(self) {
        drop(self);
    }
}

impl<M> Drop for PostSlot<'_, M> {
    fn drop(&mut self) {
        self.node.in_flight.store(false, Ordering::Release);
    }
}

struct MailboxInner<M> {
    /// Producer end: the most recently pushed node.
    tail: AtomicPtr<MailboxNode<M>>,
    /// Consumer-side placeholder, re-pushed when the queue drains so that
    /// every real node is fully unlinked before it is handed out — that is
    /// what makes node recycling (N1) legal.
    stub: MailboxNode<M>,
}

impl<M> MailboxInner<M> {
    fn stub_ptr(&self) -> *mut MailboxNode<M> {
        &self.stub as *const MailboxNode<M> as *mut MailboxNode<M>
    }

    /// # Safety
    /// `node` points to a live node that is not currently linked (N1 + N2).
    unsafe fn push_raw(&self, node: *mut MailboxNode<M>) {
        unsafe { (*node).next.store(ptr::null_mut(), Ordering::Relaxed) };
        let prev = self.tail.swap(node, Ordering::AcqRel);
        // The torn-push window is exactly here: `node` is the tail but is not
        // reachable from the consumer's head yet, so the consumer — and every
        // message pushed behind us — reads as end-of-queue until the store
        // below lands. Bounded to these two instructions by N3; an
        // interrupted (never a preempted) producer completes it before the
        // interrupted context can ever sleep, and the doorbell edge that
        // follows the push guarantees another pass (spec §7.2, §12).
        unsafe { (*prev).next.store(node, Ordering::Release) };
    }
}

/// The producer half: `Sync`, cloneable, reachable by every CPU. It can only
/// push messages — the compile-time form of "a CPU's queue is touched only by
/// its owner" (spec §6.1).
pub struct MailboxProducer<M> {
    inner: Arc<MailboxInner<M>>,
}

impl<M> Clone for MailboxProducer<M> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<M: Send> MailboxProducer<M> {
    /// Push one message. Requires a [`PostSlot`] (N1) and a live
    /// [`PreemptGuard`] (N3) — an unguarded or double push does not typecheck.
    pub fn post(&self, slot: PostSlot<'_, M>, msg: M, _preempt: &impl PreemptGuard) {
        let slot = ManuallyDrop::new(slot);
        let node = slot.node;
        // SAFETY: N1 — the claim in `slot` is exclusive and the node is not
        // published yet, so nobody else can touch the payload.
        unsafe { node.slot.put(msg) };
        let ptr = node as *const MailboxNode<M> as *mut MailboxNode<M>;
        // SAFETY: N2 — the node outlives its message; the pointer is only
        // ever used for atomic and `Slot` (UnsafeCell) accesses, never to
        // form a `&mut`.
        unsafe { self.inner.push_raw(ptr) };
    }

    /// Push a message that carries its own node — the ownership-transferring
    /// `Adopt`, which rides inside the very task record it transfers
    /// (spec §7.2). That message cannot be posted through [`Self::post`]: a
    /// [`PostSlot`] borrows the node, and the node is inside the value being
    /// moved.
    ///
    /// * **N4 (self-carried node).** `node_of(&msg)` must return a node that
    ///   lives in an allocation `msg` *owns* and that does not move when
    ///   `msg` moves — i.e. behind `Task`'s `Box`. That is what makes the
    ///   node address taken before the move still valid after it. The node
    ///   must also be free; it is claimed here, so N1 holds as for any other
    ///   push.
    ///
    /// The message owns itself while queued (the record contains the value
    /// that owns the record). Nothing else references it, so a message that
    /// is never consumed is a leak — caught by the scenario-end invariant
    /// I10, not silently absorbed.
    pub fn post_owned(
        &self,
        msg: M,
        node_of: fn(&M) -> &MailboxNode<M>,
        _preempt: &impl PreemptGuard,
    ) {
        let node = node_of(&msg) as *const MailboxNode<M> as *mut MailboxNode<M>;
        // SAFETY: N4 — `node` points into the stable allocation `msg` owns,
        // so it stays valid across the move into the slot below.
        unsafe {
            assert!(
                !(*node).in_flight.swap(true, Ordering::AcqRel),
                "a self-carried message is already in flight",
            );
            (*node).slot.put(msg);
            self.inner.push_raw(node);
        }
    }
}

/// The consumer half: `!Sync`, owned by the CPU whose mailbox it is.
pub struct MailboxConsumer<M> {
    inner: Arc<MailboxInner<M>>,
    head: *mut MailboxNode<M>,
    _not_sync: PhantomData<*mut ()>,
}

// SAFETY: `Send` but deliberately not `Sync` — the consumer end is handed to
// the CPU that owns it (boot, or a model's vcpu thread) and from then on only
// that one context touches `head`. Two contexts sharing it is what `!Sync`
// forbids, and it is also why `MailboxProducer` exists as a separate type.
unsafe impl<M: Send> Send for MailboxConsumer<M> {}

impl<M: Send> MailboxConsumer<M> {
    /// Pop one message, or `None` for "end of queue" — which includes the
    /// transient torn-push state (spec §7.2): a message behind an
    /// in-progress push is delayed by that push's remaining two instructions,
    /// never lost, because the pusher's doorbell edge follows it.
    pub fn pop(&mut self, _preempt: &impl PreemptGuard) -> Option<M> {
        let stub = self.inner.stub_ptr();
        let mut head = self.head;
        // SAFETY: `head` is a live node — the stub, or a node the queue still
        // links (N2).
        let mut next = unsafe { (*head).next.load(Ordering::Acquire) };

        if head == stub {
            if next.is_null() {
                return None;
            }
            self.head = next;
            head = next;
            // SAFETY: `next` came from the queue's links, so it is live.
            next = unsafe { (*next).next.load(Ordering::Acquire) };
        }

        if !next.is_null() {
            self.head = next;
            // SAFETY: `head` is no longer reachable from the queue — the
            // consumer has advanced past it and a successor exists, so it is
            // not the tail either.
            return Some(unsafe { self.consume(head) });
        }

        if head != self.inner.tail.load(Ordering::Acquire) {
            // A push is in flight: `head`'s successor exists but is not
            // linked yet. End of queue for this pass.
            return None;
        }

        // `head` is the last node. Re-insert the stub so that `head` becomes
        // fully unlinked and its owner may reuse it (N1).
        // SAFETY: the stub is live and, in this branch, not linked.
        unsafe { self.inner.push_raw(stub) };
        // SAFETY: `head` is live until we consume it.
        next = unsafe { (*head).next.load(Ordering::Acquire) };
        if next.is_null() {
            // A concurrent producer won the tail swap between the two loads;
            // its store will land, and the doorbell edge behind it brings us
            // back. `head` stays ours to hand out next time.
            return None;
        }
        self.head = next;
        // SAFETY: as above — `head` is unlinked and has a successor.
        Some(unsafe { self.consume(head) })
    }

    /// # Safety
    /// `node` is live, fully unlinked from the queue, and its message has not
    /// been taken.
    unsafe fn consume(&mut self, node: *mut MailboxNode<M>) -> M {
        // SAFETY: unlinked — no producer can observe the slot (N1).
        let msg = unsafe { (*node).slot.take() }.expect("linked node without a message");
        // Release *after* the unlink, so a producer that observes the node
        // free (claim succeeds) also observes it out of the queue — the
        // §7.7 steal-probe recycling rule, generalized to every node.
        unsafe { (*node).in_flight.store(false, Ordering::Release) };
        msg
    }

    /// Would [`Self::pop`] return `None` right now? Conservative in exactly
    /// one direction: an in-progress push reads as empty, which is what makes
    /// the sleep handshake's ordering (SLEEPING before this check) load
    /// bearing (spec §7.5).
    pub fn is_empty(&self) -> bool {
        let stub = self.inner.stub_ptr();
        if self.head != stub {
            return false;
        }
        // SAFETY: the stub lives in the shared allocation for as long as both
        // halves do.
        unsafe { (*stub).next.load(Ordering::Acquire) }.is_null()
    }
}

/// Create a mailbox and split it into its two halves.
pub fn mailbox<M: Send>() -> (MailboxProducer<M>, MailboxConsumer<M>) {
    let inner = Arc::new(MailboxInner {
        tail: AtomicPtr::new(ptr::null_mut()),
        stub: MailboxNode::new(),
    });
    let stub = inner.stub_ptr();
    inner.tail.store(stub, Ordering::Relaxed);
    (
        MailboxProducer {
            inner: inner.clone(),
        },
        MailboxConsumer {
            inner,
            head: stub,
            _not_sync: PhantomData,
        },
    )
}

/// How promptly the target must notice a posted message (spec §7.3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Urgency {
    /// RT wake, boost wake, adopt of an RT task, retire: the target must
    /// preempt, so the IPI is unconditional.
    Preempt,
    /// Ordinary wake: a busy target drains at its next safe point (≤ one
    /// quantum, matching today's contract) and needs no interrupt; a sleeping
    /// target is always kicked.
    Normal,
}

/// Whether the producer must send the targeted IPI.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[must_use = "an elided kick is a decision; a required kick must be sent"]
pub enum Kick {
    Send,
    Elide,
}

const KICK_PENDING: u32 = 1 << 0;
const SLEEPING: u32 = 1 << 1;

/// The per-CPU doorbell: the kick-pending edge plus the sleeping bit that
/// makes the idle handshake safe (spec §7.3, §7.5).
pub struct Doorbell {
    bits: AtomicU32,
}

impl Doorbell {
    pub fn new() -> Self {
        Self {
            bits: AtomicU32::new(0),
        }
    }

    /// Producer side, immediately after [`MailboxProducer::post`]. Returns
    /// whether a targeted IPI is required.
    pub fn ring(&self, urgency: Urgency) -> Kick {
        let prev = self.bits.fetch_or(KICK_PENDING, Ordering::AcqRel);
        match urgency {
            // Unconditional: a prior normal-wake edge may have elided its IPI.
            Urgency::Preempt => Kick::Send,
            // Edge-coalesced: only the producer that raises the 0→1 edge on a
            // sleeping target kicks it. At 128 cores a normal wake to a busy
            // CPU costs zero IPIs.
            Urgency::Normal => {
                if prev & SLEEPING != 0 && prev & KICK_PENDING == 0 {
                    Kick::Send
                } else {
                    Kick::Elide
                }
            }
        }
    }

    /// Consumer side, at pass start: clear the edge *before* draining, so a
    /// message posted after the drain re-raises it (spec §7.3). Also clears
    /// SLEEPING — a CPU running a pass is by definition awake.
    pub fn begin_pass(&self) {
        self.bits
            .fetch_and(!(KICK_PENDING | SLEEPING), Ordering::AcqRel);
    }

    /// Consumer side, idle path: publish SLEEPING *before* the final
    /// mailbox-empty check. Any message not seen by that check has its
    /// doorbell edge after this store, so its producer sees SLEEPING and
    /// kicks (spec §7.5).
    pub fn arm_sleep(&self) -> SleepArm<'_> {
        self.bits.fetch_or(SLEEPING, Ordering::AcqRel);
        SleepArm { doorbell: self }
    }

    /// The driver's last look before `hlt`, under IRQs off.
    pub fn kick_pending(&self) -> bool {
        self.bits.load(Ordering::Acquire) & KICK_PENDING != 0
    }

    pub fn sleeping(&self) -> bool {
        self.bits.load(Ordering::Acquire) & SLEEPING != 0
    }
}

impl Default for Doorbell {
    fn default() -> Self {
        Self::new()
    }
}

/// SLEEPING is published; the final mailbox check has not happened yet. The
/// only way to obtain a [`crate::cpu::SleepToken`] runs through here, so "halt with work
/// queued" has no expression (spec §7.5).
#[must_use = "an armed sleep must be confirmed or abandoned"]
pub struct SleepArm<'d> {
    doorbell: &'d Doorbell,
}

/// A message arrived (or was already there): stay awake and run another pass.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Awake;

/// SLEEPING was published before a mailbox-empty check that came back empty.
/// Half of what [`crate::cpu::SleepToken`] requires.
#[must_use]
pub struct Quiesced {
    _private: (),
}

impl SleepArm<'_> {
    /// The final check. `mailbox` is this CPU's consumer: passing someone
    /// else's is impossible, since a `MailboxConsumer` is `!Send` and lives
    /// in the CPU's own `CpuSched`.
    ///
    /// Success yields [`Quiesced`], one of the two halves [`crate::cpu::SleepToken`]
    /// needs — the other being the applied timer plan (spec §7.5, §8.4). A
    /// CPU therefore cannot halt with work queued *or* with a deadline
    /// pending and the timer unarmed, and neither fact is asserted anywhere:
    /// there is no way to say it.
    pub fn confirm<M: Send>(self, mailbox: &MailboxConsumer<M>) -> Result<Quiesced, Awake> {
        if !mailbox.is_empty() || self.doorbell.kick_pending() {
            return Err(Awake);
        }
        Ok(Quiesced { _private: () })
    }

    /// Give up on sleeping without checking (a pass decided otherwise).
    pub fn abandon(self) {
        drop(self);
    }
}

/// The guard the crate's own unit tests push under. It lives here, in the
/// one module allowed to write `unsafe`, so that no other module needs an
/// exemption to implement [`PreemptGuard`].
#[cfg(test)]
pub(crate) struct NoPreempt;

// SAFETY: the unit tests are single-threaded and run no scheduler, so the
// executing context cannot be descheduled. Concurrency is loom's job
// (`toyos-sched/loom/`), where the guard models the real preempt count.
#[cfg(test)]
unsafe impl PreemptGuard for NoPreempt {}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Eq)]
    struct Msg(u32);

    #[test]
    fn fifo_across_nodes_and_recycling() {
        let (tx, mut rx) = mailbox::<Msg>();
        let a = MailboxNode::new();
        let b = MailboxNode::new();
        assert!(rx.is_empty());

        tx.post(a.claim().unwrap(), Msg(1), &NoPreempt);
        tx.post(b.claim().unwrap(), Msg(2), &NoPreempt);
        assert!(!rx.is_empty());
        assert_eq!(rx.pop(&NoPreempt), Some(Msg(1)));
        assert_eq!(rx.pop(&NoPreempt), Some(Msg(2)));
        assert_eq!(rx.pop(&NoPreempt), None);
        assert!(rx.is_empty());

        // Both nodes are free again — the stub re-push unlinked them.
        assert!(!a.in_flight() && !b.in_flight());
        tx.post(a.claim().unwrap(), Msg(3), &NoPreempt);
        assert_eq!(rx.pop(&NoPreempt), Some(Msg(3)));
    }

    #[test]
    fn a_node_carries_one_message_at_a_time() {
        let (tx, mut rx) = mailbox::<Msg>();
        let n = MailboxNode::new();
        let slot = n.claim().expect("first claim");
        assert!(n.claim().is_none(), "I12: one message in flight per node");
        tx.post(slot, Msg(7), &NoPreempt);
        assert!(n.claim().is_none(), "still queued");
        assert_eq!(rx.pop(&NoPreempt), Some(Msg(7)));
        assert!(n.claim().is_some(), "consumed: the node is free again");
    }

    #[test]
    fn releasing_an_unposted_claim_frees_the_node() {
        let n = MailboxNode::<Msg>::new();
        n.claim().expect("claim").release();
        assert!(n.claim().is_some(), "released");
        // Dropping the claim releases it too, so a forgotten post cannot
        // wedge the node.
        assert!(!n.in_flight());
    }

    #[test]
    #[should_panic(expected = "message in flight")]
    fn dropping_a_queued_node_is_loud() {
        let (tx, _rx) = mailbox::<Msg>();
        let n = MailboxNode::new();
        tx.post(n.claim().unwrap(), Msg(1), &NoPreempt);
        drop(n);
    }

    #[test]
    fn doorbell_kick_policy() {
        let d = Doorbell::new();
        // Busy target, ordinary wake: no IPI.
        assert_eq!(d.ring(Urgency::Normal), Kick::Elide);
        d.begin_pass();
        // Sleeping target: the 0→1 edge kicks, coalesced edges do not.
        d.arm_sleep().abandon();
        assert_eq!(d.ring(Urgency::Normal), Kick::Send);
        assert_eq!(d.ring(Urgency::Normal), Kick::Elide);
        // Preempt urgency never elides.
        assert_eq!(d.ring(Urgency::Preempt), Kick::Send);
        d.begin_pass();
        assert!(!d.kick_pending() && !d.sleeping());
    }

    #[test]
    fn sleep_needs_an_empty_mailbox_and_a_quiet_doorbell() {
        let (tx, mut rx) = mailbox::<Msg>();
        let n = MailboxNode::new();
        let d = Doorbell::new();

        tx.post(n.claim().unwrap(), Msg(1), &NoPreempt);
        assert!(d.arm_sleep().confirm(&rx).is_err(), "work queued");
        assert_eq!(rx.pop(&NoPreempt), Some(Msg(1)));

        // Drained, but a producer rang after the drain: still no sleep.
        assert_eq!(d.ring(Urgency::Normal), Kick::Send);
        assert!(d.arm_sleep().confirm(&rx).is_err(), "edge pending");
        d.begin_pass();
        assert!(d.arm_sleep().confirm(&rx).is_ok());
    }
}
