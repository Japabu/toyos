//! The record, and the bounded ring a waiter owns.
//!
//! **This file is compiled a second time by `kernel-loom`**, so it may name
//! only what that crate supplies: the atomics, the cell, `toyos_abi`'s error
//! type and `crate::time`. That is a layout requirement rather than a style
//! rule, and it is why `Subject`, `Watch` and `arm` live one level up in
//! `mod.rs`, where they
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
//! (`issues/hardware/one-rmw-per-log-line-cost-350ms.md`), because QEMU
//! cannot always emit an inline host atomic for a guest RMW. So `tail` is a
//! plain load and a plain store made under the lock the poster already holds
//! (§16.2 rule 1), `head` is the same in the taker's hand, and the overflow
//! count is a load and a store rather than an increment. What makes the plain
//! stores sound is stated as an invariant and asserted at the arm:
//!
//! - **One poster at a time, and the mechanism is the subject's leaf lock.**
//!   Every post to a subject walks its watch list under that lock, so the
//!   posters to one inbox are serialized by construction. A producer that
//!   cannot take a lock therefore cannot use [`Inbox::post`] at all, and has
//!   [`Inbox::signal`] instead.
//! - **One taker, ever.** The inbox belongs to one task and only that task
//!   takes from it.
//!
//! **The struck version of the first bullet named the wrong mechanism, and the
//! difference was undefined behaviour.** It argued from "an inbox is armed on
//! exactly one subject (§5.3's `Armed` is consumed by the wait), and `arm`
//! refuses a second arm by name" — a signature this implementation does not
//! have (the arm is held by reference across a loop, §5.3a) and, more
//! importantly, an argument about *subjects* where the invariant is about
//! *posters*. The log's producer proved the gap: it posts to `klogd`'s inbox
//! with no lock at all, admitted one-per-*epoch* by `shard::signal_after_commit`'s
//! swap — and `klogd`'s re-arm-then-continue loop starts a new epoch while the
//! previous epoch's producer is still inside `post`. Two CPUs writing one
//! `UnsafeCell<Record>` is not a lost record, it is UB.
//!
//! The two read-modify-writes in the file are both flags, both on the *taker's*
//! side of a word both sides write: the overflow notice and the signal. Neither
//! can be a load and a store, and a lost one is a lost wake rather than a lost
//! record.

#[cfg(not(feature = "loom"))]
use core::cell::UnsafeCell;
#[cfg(not(feature = "loom"))]
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

#[cfg(feature = "loom")]
use crate::cell::UnsafeCell;
#[cfg(feature = "loom")]
use loom::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

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

    /// The opaque value back, for the one place that has to store a token in a
    /// word: [`Inbox::arm_to`].
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Why a subject is gone. Never a bare timeout — the reason is the value.
///
/// **Three of §5.1's four, each arriving with its first producer.** `Revoked`
/// — a device claim released out from under a waiter — is C7's, because that
/// is the chunk with the claim to release; a variant nothing constructs is
/// dead code this tree's build refuses.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// The subject ended: the peer's last handle closed, or the object told
    /// its waiters it will never answer.
    Closed,
    /// The caller's own deadline passed. Never a bare timeout — the reason is
    /// the value, and whose business the expiry is comes from `Deadline`.
    Expired,
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
    /// [`Inbox::signal`]'s whole state: "something happened", from a producer
    /// that may take no lock. Written by any number of them concurrently and
    /// cleared by the owner, which is sound because it is one atomic word and
    /// not a `Record`.
    signalled: AtomicBool,
    /// Whether an [`Armed`](super::Armed) is live for this inbox. Written only
    /// by the owner.
    armed: AtomicBool,
    /// The token the live arm named, so [`Inbox::signal`]'s contentless notice
    /// can be handed to the taker as a record of the subject it is actually
    /// waiting on. Written only by the owner, at the arm.
    armed_token: AtomicU64,
}

// SAFETY: every slot is written under the posting subject's own leaf lock — the
// walk of its watch list is what serializes the posters to one inbox — and read
// by the single owner, and the `tail`/`head` release-acquire pair is what orders
// the two. A producer that cannot take that lock has `signal` instead, which
// touches no slot at all. The module header states both halves of the invariant
// and the mechanism each rests on.
unsafe impl Sync for Inbox {}

impl Default for Inbox {
    fn default() -> Self {
        Self::new()
    }
}

impl Inbox {
    #[cfg(not(feature = "loom"))]
    pub const fn new() -> Self {
        Self {
            slots: [const { UnsafeCell::new(Record::EMPTY) }; MAX_INBOX],
            tail: AtomicU32::new(0),
            head: AtomicU32::new(0),
            overflowed: AtomicBool::new(false),
            signalled: AtomicBool::new(false),
            armed: AtomicBool::new(false),
            armed_token: AtomicU64::new(0),
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
            signalled: AtomicBool::new(false),
            armed: AtomicBool::new(false),
            armed_token: AtomicU64::new(0),
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

    /// Say that something happened, without saying what — the form a producer
    /// that may take no lock is allowed to use.
    ///
    /// **One atomic store, and that is the whole point.** [`Inbox::post`]'s
    /// plain writes are sound only because the posters to one subject are
    /// serialized by that subject's leaf lock; a producer with no lock has no
    /// such serialization and a second one racing it is undefined behaviour
    /// rather than a lost record. The machine's log is that producer, by
    /// necessity: `emit` runs inside `sync.rs`, inside IRQ handlers, inside the
    /// scheduler and inside every syscall's locked region, and one
    /// read-modify-write per log line measured 350 ms of boot under TCG.
    ///
    /// What it costs the reader is exactness: a signal carries no `at` and no
    /// outcome of its own, which is §5.3a's *edge* contract stated as a type —
    /// the record means "state may have moved", never "there is something for
    /// you", and the waiter's own predicate is authoritative. That is what the
    /// log's reader does anyway.
    pub fn signal(&self) {
        self.signalled.store(true, Ordering::Release);
    }

    /// Is there anything for the owner? **The one park-time recheck**, and one
    /// predicate: no match on a channel, no per-source closure, nothing named
    /// in it.
    pub fn has_record(&self) -> bool {
        self.tail.load(OBSERVE) != self.head.load(Ordering::Relaxed)
            || self.overflowed.load(Ordering::Acquire)
            || self.signalled.load(Ordering::Acquire)
    }

    /// Take the oldest record. Owner only.
    pub fn take(&self) -> Option<Record> {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(OBSERVE);
        if head == tail {
            // A signal with nothing in the ring is the log's shape: a record
            // with no content, carrying the token of whatever this inbox is
            // armed on, because that is the subject whose state may have moved.
            if self.signalled.swap(false, Ordering::AcqRel) {
                return Some(Record {
                    token: Token::new(self.armed_token.load(Ordering::Relaxed)),
                    outcome: Outcome::Ready,
                    at: Instant::from_nanos_since_boot(0),
                });
            }
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

    /// Whether an arm is live. Only the owner writes it.
    pub fn is_armed(&self) -> bool {
        self.armed.load(Ordering::Relaxed)
    }

    /// Take the arm, naming the subject it is for.
    ///
    /// **It deliberately does not empty the ring**, and the struck version did.
    /// "A new wait starts owing nothing" is true and is enforced at the other
    /// end — [`Armed`](super::Armed)'s `Drop` drains, under the same leaf lock
    /// that stops any further post reaching this inbox — so a reset here can
    /// only discard something that arrived *between* the two, which for a
    /// lock-free signaller is a wake nobody will send again. A record that
    /// outlives its arm costs the next wait one spurious loop, which is legal
    /// at every park site (§5.5).
    ///
    /// `pub` rather than `pub(super)` so that `kernel-loom`, where this file's
    /// `super` is a different crate root, still sees a used item. `mod.rs` is
    /// its only caller in the kernel.
    pub fn arm_to(&self, token: Token) {
        self.armed_token.store(token.raw(), Ordering::Relaxed);
        self.armed.store(true, Ordering::Relaxed);
    }

    /// Give the arm back. The drain is the caller's, because it is the caller
    /// that holds the subject's lock while it happens.
    pub fn disarm(&self) {
        self.armed.store(false, Ordering::Relaxed);
    }
}
