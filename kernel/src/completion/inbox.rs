//! The record, and the bounded ring a waiter owns.
//!
//! **This file is compiled a second time by `kernel-loom`**, so it may name
//! only what that crate supplies: the atomics, the cell, `toyos_abi`'s error
//! type and `crate::time`. That is a layout requirement rather than a style
//! rule — `specs/completion-architecture-spec.md` §16.1 states it — and it is
//! why `Subject`, `Watch` and `arm` live one level up in `mod.rs`, where they
//! may name pipe ends and device claims. x86's TSO gives every load acquire and
//! every store release semantics, so a missing edge here is invisible to every
//! guest test; loom is the only instrument in the tree that can see one, and
//! ARM64 is planned.
//!
//! **The ordering, in one sentence.** A poster writes the slot and *then*
//! publishes `tail` with a release; a taker reads `tail` with an acquire and
//! only then reads the slot. That pair is the whole of the record's
//! publication, and `kernel-loom/tests/inbox.rs` is what proves it — with the
//! release removed, the model must red.
//!
//! **No read-modify-write on the post path, and that is a measured
//! constraint.** One `fetch_add` per log line cost 350 ms of boot under TCG
//! (`specs/issues/hardware/one-rmw-per-log-line-cost-350ms.md`), because QEMU
//! cannot always emit an inline host atomic for a guest RMW. So `tail` is a
//! plain load and a plain store made under the lock the poster already holds
//! (§16.2 rule 1), `head` is the same in the taker's hand, and the overflow
//! count is a load and a store rather than an increment. What makes the plain
//! stores sound is stated as an invariant and asserted at the arm:
//!
//! - **One poster at a time.** An inbox is armed on exactly one subject
//!   (§5.3's `Armed` is consumed by the wait), and every post to that subject
//!   walks its watch list under the subject's own leaf lock. `arm` refuses a
//!   second arm by name, so the invariant is checked rather than hoped for.
//! - **One taker, ever.** The inbox belongs to one task and only that task
//!   takes from it.
//!
//! The one read-modify-write in the file is the overflow flag's `swap`, on the
//! taker's side and only when the ring is already empty. It cannot be a load
//! and a store because both sides write that flag, and a lost overflow is a
//! lost wake rather than a lost record.

#[cfg(not(feature = "loom"))]
use core::cell::UnsafeCell;
#[cfg(not(feature = "loom"))]
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

#[cfg(feature = "loom")]
use crate::cell::UnsafeCell;
#[cfg(feature = "loom")]
use loom::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::time::Instant;

/// The store that publishes a record, and the load that observes one.
///
/// **A cargo feature rather than a comment, because a model that has never
/// failed proves nothing.** `kernel-loom`'s `inbox-release-off` makes both
/// relaxed and `kernel-loom/tests/inbox.rs` must red under it — the slot write
/// is then unordered against the taker's read, which is exactly the class x86's
/// TSO hides from every guest test in this tree. No kernel build can turn it
/// on: the kernel declares the name only so `cfg` checking knows it.
#[cfg(not(feature = "inbox-release-off"))]
const PUBLISH: Ordering = Ordering::Release;
#[cfg(feature = "inbox-release-off")]
const PUBLISH: Ordering = Ordering::Relaxed;
#[cfg(not(feature = "inbox-release-off"))]
const OBSERVE: Ordering = Ordering::Acquire;
#[cfg(feature = "inbox-release-off")]
const OBSERVE: Ordering = Ordering::Relaxed;

/// Records an inbox holds before it starts dropping them.
///
/// Eight, and the number is a ceiling on *unclaimed* records rather than on
/// concurrency: a waiter takes what it is woken for, so the ring only fills
/// when something posts repeatedly to a task that is not running. Overflow is
/// a bounded loss the waiter is told about, never a lost wake — see
/// [`Inbox::post`].
///
/// **Two under loom**, for `shard.rs`'s reason: a model that had to post eight
/// records to reach the full case would explore branches it does not need, and
/// nothing the models check depends on the value.
#[cfg(not(feature = "loom"))]
pub const MAX_INBOX: usize = 8;
#[cfg(feature = "loom")]
pub const MAX_INBOX: usize = 2;

/// Chosen by the waiter when it armed. Opaque here: the completion core maps no
/// id to any object, so nothing in a record can name a freed one.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct Token(u64);

impl Token {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }
}

/// Why a subject is gone. Never a bare timeout — the reason is the value.
///
/// **One variant, and the rest arrive with their first producer.** §5.1's set
/// also has the peer's last handle closing, a device claim revoked and a
/// deadline passing; C2 posts none of those, and a variant nothing constructs
/// is dead code this tree's build refuses. C3's cancellers bring `Closed` and
/// `Expired`, C7's device claim brings `Revoked`, and each arrives beside the
/// code that produces it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// The inbox filled while this waiter was not running. The record it
    /// replaces is lost; the waiter re-derives its own predicate, which is
    /// legal at every park site (§5.5).
    Overflowed,
}

/// What happened. The consumer must match: there is no `Option`, and no value
/// that means "nothing to say".
///
/// One shape for every wait is the whole argument — a caller cannot handle a
/// disk's refusal and a pipe's differently by accident — and `Gone` makes "the
/// subject went away" a value rather than an absence.
///
/// `Moved(u32)` for the bytes a transfer actually moved and
/// `Failed(SyscallError)` for a device that said no are §5.1's other two, and
/// they land with C7's transfer and C3's refusals for the reason [`Reason`]
/// gives.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ready,
    Gone(Reason),
}

/// A record that something happened.
#[derive(Clone, Copy)]
pub struct Record {
    pub token: Token,
    pub outcome: Outcome,
    /// When the *event* happened, not when it was drained. A post stamps it on
    /// the CPU that observed the event.
    pub at: Instant,
}

impl Record {
    /// The zeroed state a slot starts in. Never taken: `head == tail` is what
    /// says a slot holds nothing, and no reader looks past that.
    const EMPTY: Self = Self {
        token: Token::new(0),
        outcome: Outcome::Ready,
        at: Instant::from_nanos_since_boot(0),
    };
}

/// A bounded ring of records, owned by whoever waits.
///
/// **Level-readable, and that is a property of the record rather than of the
/// subject that posted it** (§5.3a): a record stays until its owner takes it,
/// so a post that lands between a waiter's last look and its park is found by
/// the park's own recheck. That is what collapses the recheck to one predicate
/// — [`Inbox::has_record`] — with nothing named in it.
pub struct Inbox {
    slots: [UnsafeCell<Record>; MAX_INBOX],
    /// Written only by a poster, under the subject's leaf lock; read by the
    /// owner with an acquire.
    tail: AtomicU32,
    /// Written only by the owner; read by a poster to see how much room it has.
    head: AtomicU32,
    /// Set by a poster that found the ring full, cleared by the taker that
    /// reports it. A flag rather than a count: what the waiter does about it
    /// is re-derive its predicate, and it does that once.
    overflowed: AtomicBool,
    /// Whether an [`Armed`](super::Armed) is live for this inbox. Written only
    /// by the owner, and the one-poster-at-a-time invariant this file's plain
    /// stores rest on.
    armed: AtomicBool,
}

// SAFETY: every slot is written by the single poster the arm admits and read by
// the single owner, and the `tail`/`head` release-acquire pair is what orders
// the two. The module header states both halves of that invariant.
unsafe impl Sync for Inbox {}

impl Inbox {
    #[cfg(not(feature = "loom"))]
    pub const fn new() -> Self {
        Self {
            slots: [const { UnsafeCell::new(Record::EMPTY) }; MAX_INBOX],
            tail: AtomicU32::new(0),
            head: AtomicU32::new(0),
            overflowed: AtomicBool::new(false),
            armed: AtomicBool::new(false),
        }
    }

    /// Loom's atomics have no const constructor — `sync.rs`'s second arm, for
    /// the same reason.
    #[cfg(feature = "loom")]
    pub fn new() -> Self {
        Self {
            slots: [(); MAX_INBOX].map(|()| UnsafeCell::new(Record::EMPTY)),
            tail: AtomicU32::new(0),
            head: AtomicU32::new(0),
            overflowed: AtomicBool::new(false),
            armed: AtomicBool::new(false),
        }
    }

    /// Store a record. **Called only with the subject's leaf lock held**, which
    /// is what makes the plain `tail` store sound.
    ///
    /// A full ring drops the record and raises [`Reason::Overflowed`] instead:
    /// a bounded loss, never a lost wake, because the waiter that reads it
    /// re-derives its own predicate.
    pub fn post(&self, record: Record) {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        if tail.wrapping_sub(head) as usize >= MAX_INBOX {
            self.overflowed.store(true, Ordering::Release);
            return;
        }
        let slot = tail as usize % MAX_INBOX;
        // SAFETY: this poster owns the slot until `tail` publishes it — the
        // owner does not read past `tail`, and no second poster exists while
        // the arm holds.
        unsafe { self.slots[slot].get().write(record) };
        self.tail.store(tail.wrapping_add(1), PUBLISH);
    }

    /// Is there anything for the owner? **The one park-time recheck**, and one
    /// predicate: no match on a channel, no per-source closure, nothing named
    /// in it.
    pub fn has_record(&self) -> bool {
        self.tail.load(OBSERVE) != self.head.load(Ordering::Relaxed)
            || self.overflowed.load(Ordering::Acquire)
    }

    /// Take the oldest record. Owner only.
    pub fn take(&self) -> Option<Record> {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(OBSERVE);
        if head == tail {
            // An overflow with nothing left in the ring is still something the
            // waiter has to hear about, once.
            return self.overflowed.swap(false, Ordering::AcqRel).then(|| Record {
                token: Token::new(0),
                outcome: Outcome::Gone(Reason::Overflowed),
                at: Instant::from_nanos_since_boot(0),
            });
        }
        let slot = head as usize % MAX_INBOX;
        // SAFETY: `tail` was published with a release after this slot was
        // written, and the acquire above is its pair.
        let record = unsafe { self.slots[slot].get().read() };
        self.head.store(head.wrapping_add(1), Ordering::Release);
        Some(record)
    }

    /// Empty it, so a new wait starts with nothing owed. The previous wait's
    /// records belong to that wait.
    pub fn reset(&self) {
        while self.has_record() {
            let _ = self.take();
        }
    }

    /// Whether an arm is live. Only the owner writes it.
    pub fn is_armed(&self) -> bool {
        self.armed.load(Ordering::Relaxed)
    }

    /// `pub` rather than `pub(super)` so that `kernel-loom`, where this file's
    /// `super` is a different crate root, still sees a used item. `mod.rs` is
    /// its only caller in the kernel.
    pub fn set_armed(&self, armed: bool) {
        self.armed.store(armed, Ordering::Relaxed);
    }
}
