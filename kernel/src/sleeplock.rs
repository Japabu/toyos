//! The lock a contender parks on.
//!
//! `specs/completion-architecture-spec.md` §8. One sentence: **a CPU never
//! spins on a lock whose holder is descheduled** — a contender takes a ticket,
//! arms on the lock's own watch and gives the CPU back, and the release posts to
//! the one whose turn it now is.
//!
//! **Preemption stays on for the holder, and that is the whole point.** A
//! [`crate::sync::Lock`] guard raises the preempt count for its whole life, so
//! its holder cannot be descheduled and a lock held across a device round trip
//! pins a CPU for the length of that trip — which is §1.1's finding, four
//! spinlocks deep at the moment of a disk transfer. A [`SleepGuard`] raises
//! nothing, so `scheduler::assert_baseline` keeps meaning exactly what it means
//! today: *a spinlock is held*.
//!
//! **Nothing in the kernel holds one yet, and C7+C8 is what changes that.**
//! §21's C5 row is "`SleepLock`, `holder()`, the `Parkable` threading. Nothing
//! converted" — the four statics of §9 (`vfs::VFS`, `fat32_adapter::VOLUMES`,
//! `xhci::XHCI`, `process::ProcessData`) convert in one later chunk, because
//! converting any one of them alone parks with the other three still ticket
//! locks and trips `assert_baseline` by construction (§21.1). So this module is
//! `allow(dead_code)` for exactly one chunk, and the allow names it. What keeps
//! it honest in the meantime is not the kernel but `kernel-loom`, which
//! compiles this file a second time and drives the real acquire.
//!
//! **This file is compiled a second time by `kernel-loom`**, so it may name only
//! what that crate supplies — §16.1's layout requirement, the same one
//! `completion::inbox` carries. Seven items in two modules
//! (`completion::{Outcome, Subject, Token, Watch, wait_uncancellable_until}`,
//! `scheduler::{current_task, Parkable, TaskId}`), all of them either compiled
//! there already or three lines of arithmetic. A file that named a pipe end or
//! a device claim could not be modelled at all, and on x86 — where every load is
//! an acquire — a missing edge here is invisible to every guest test.
//!
//! ## Why a ticket and not a bit
//!
//! One word, CAS `FREE` → me, and a release that posts to *everyone* armed, is
//! shorter. It is also a thundering herd, and worse, it starves: a woken
//! contender that loses the race to a caller arriving fresh on another CPU goes
//! back to sleep with nothing owed to it, for ever. A ticket makes the wake
//! *addressed* — the contender arms with its own ticket as the completion token
//! and the release posts to that token alone, which is [`completion::post_n`]'s
//! existing shape and the reason it takes a token at all. One waiter wakes, it
//! is the one whose turn it is, and the order is the order they arrived in.
//!
//! ## What it costs, counted rather than asserted
//!
//! One read-modify-write per uncontended acquire and one per uncontended
//! release — the same two `Lock` pays — plus one plain store of the holder on
//! each side and one relaxed load of the watch's arm count on the release. That
//! surcharge is the whole of what [`SleepLock::holder`] and the addressed wake
//! cost a lock nobody is waiting on. The contended paths add four RMWs to the
//! acquirer and three to the releaser, all of them on the far side of a park.
//! Nothing on either path is a `fetch_add` on a *count*, which is
//! `specs/completion-architecture-spec.md` §16.2's first rule and the reason it
//! exists: one such increment per log line cost 350 ms of boot under TCG.
//!
//! ## The two rules an acquirer has to know
//!
//! * **A `SleepLock` taken while a [`crate::sync::Lock`] is held must go through
//!   [`SleepLock::try_lock`]** — [`Parkable::of_current`] asserts the context's
//!   baseline preempt depth, so the token a park needs cannot be made at that
//!   depth. That is §8's ordering rule and it is enforced rather than reviewed.
//!   The converse is free: a `Lock` may be taken while a `SleepGuard` is held.
//! * **A `SleepLock` may not be acquired from inside an armed wait's
//!   predicate.** `completion::arm` refuses a second arm on one inbox by name,
//!   and a contended acquire arms. So the `ready` closure a caller hands
//!   `completion::wait_until` may take a `try_lock` and may not take a `lock`.
//!   Nothing in the tree does either today; it is written down because the
//!   failure is a named panic at a call site that looks innocent.

// **Retired by C7+C8, which is the chunk that gives this type its first
// holder.** The allow names the chunk rather than standing open: every item
// below has a caller in `kernel-loom/tests/sleep_lock.rs` today and none in the
// kernel, and §21 makes that a property of the chunking — a lock converted
// alone parks with the other three of §9 still ticket locks and trips
// `assert_baseline` by construction (§21.1). Deleting it and writing it again
// at C7+C8 would land the primitive and its four conversions in one commit,
// which is the merge this pipeline's own §24.7 is about.
#![allow(dead_code)]

#[cfg(not(feature = "loom"))]
use core::cell::UnsafeCell;
#[cfg(not(feature = "loom"))]
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

#[cfg(feature = "loom")]
use crate::cell::UnsafeCell;
#[cfg(feature = "loom")]
use loom::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use core::ops::{Deref, DerefMut};

use crate::completion::{self, Outcome, Subject, Token, Watch};
use crate::scheduler::{current_task, Parkable, TaskId};

/// The load that decides whose turn it is, and what it carries.
///
/// The atomic a release publishes through is [`SleepLock::now`], so whichever
/// operation reads `now` is the one that has to carry the acquire — an acquire
/// on `ticket` would synchronize with nothing, because nothing ever releases to
/// `ticket`. `sync.rs`'s `ACQUIRED` is the same edge in the same shape, and its
/// doc carries the same argument.
///
/// **A cargo feature rather than a comment, because a model that has never
/// failed proves nothing.** `kernel-loom`'s `sleeplock-acquire-off` makes both
/// reads `Relaxed` and `kernel-loom/tests/sleep_lock.rs` must red under it: the
/// previous holder's writes are then unordered against the next holder's reads,
/// which is exactly the class x86's TSO hides from every guest test in this
/// tree. No kernel build can turn it on — the kernel declares the name only so
/// `cfg` checking knows it.
#[cfg(not(feature = "sleeplock-acquire-off"))]
const TURN: Ordering = Ordering::Acquire;
#[cfg(feature = "sleeplock-acquire-off")]
const TURN: Ordering = Ordering::Relaxed;

/// Nobody holds it.
///
/// `u32::MAX` in the pid half, which `id_map` never issues — its ids are
/// monotonic from zero and a machine that reached four billion processes has a
/// different problem. `sched::kthread`'s rows already rest on the same
/// reservation.
const FREE: u64 = u64::MAX;

/// Held by a context that is not a task: boot, through [`SleepLock::try_lock`].
///
/// Distinct from [`FREE`] because the two are different facts, even though
/// [`SleepLock::holder`] answers `None` to both — see its doc.
const NOT_A_TASK: u64 = u64::MAX - 1;

fn word_of(task: Option<TaskId>) -> u64 {
    match task {
        Some(id) => id.pack(),
        None => NOT_A_TASK,
    }
}

/// A lock whose contended acquire parks.
pub struct SleepLock<T> {
    /// The next ticket to hand out. An uncontended acquire CASes it from
    /// [`Self::now`], which is what makes "free, and nobody queued" one
    /// question.
    ticket: AtomicU32,
    /// Whose turn it is. The release publishes the critical section through
    /// this, and [`TURN`] is the load that observes it.
    now: AtomicU32,
    /// [`FREE`], [`NOT_A_TASK`], or the holder's packed [`TaskId`].
    ///
    /// **Not part of the acquire.** Mutual exclusion is the ticket pair above;
    /// this word is written after the lock is won and cleared before it is
    /// released, and its only readers are [`SleepLock::holder`] and the
    /// self-deadlock refusal. §16.2 prices it: one plain store per acquire and
    /// one per release, on top of the two read-modify-writes a `Lock` already
    /// pays.
    holder: AtomicU64,
    /// Contenders arm here, each with its own ticket as the token, so the
    /// release wakes exactly one and it is the right one.
    watch: Watch,
    data: UnsafeCell<T>,
}

// SAFETY: the ticket pair grants exclusive access to `data` — a caller holds a
// `SleepGuard` only between winning its turn and the release that publishes it —
// and `T: Send` because any task may end up the holder.
unsafe impl<T: Send> Sync for SleepLock<T> {}

impl<T> SleepLock<T> {
    /// Every converted lock in the kernel is a `static`, so this must stay
    /// `const`. Loom's atomics have no const constructor, hence the second arm —
    /// `sync.rs` splits for the same reason.
    #[cfg(not(feature = "loom"))]
    pub const fn new(value: T) -> Self {
        Self {
            ticket: AtomicU32::new(0),
            now: AtomicU32::new(0),
            holder: AtomicU64::new(FREE),
            watch: Watch::new(),
            data: UnsafeCell::new(value),
        }
    }

    #[cfg(feature = "loom")]
    pub fn new(value: T) -> Self {
        Self {
            ticket: AtomicU32::new(0),
            now: AtomicU32::new(0),
            holder: AtomicU64::new(FREE),
            watch: Watch::new(),
            data: UnsafeCell::new(value),
        }
    }

    /// Acquire, parking if it is held.
    ///
    /// **The park is uncancellable, and §7.4 is the whole reason.** A killed
    /// thread's teardown takes `ProcessData` and then the VFS through
    /// `ops::close_all`, so a kill that ended a lock acquire would either leave
    /// the teardown unable to acquire anything — `WaitTicket::commit` refuses to
    /// park a killed task on an ordinary ticket — or hand the caller a second
    /// `Cancelled` and trip RT4. What ends this wait is the holder's own
    /// release, which is bounded because a killed *holder* runs its unwind and
    /// drops the guard on the way out (§7.2); `retire_task`'s tripwire prices
    /// "every sleep-lock acquire on the way" for exactly this reason.
    ///
    /// So there is no `Result` here and no `?` at any call site, which is also
    /// what §8's signature says.
    #[track_caller]
    pub fn lock<'p>(&'p self, p: &'p Parkable) -> SleepGuard<'p, T> {
        let me = current_task();
        // **RT6.** A second acquire by the holder would take a ticket nothing
        // will ever serve, and the thread would park until `retire_task`'s
        // tripwire fired somewhere else entirely. Naming it here names the call
        // site that asked.
        assert!(
            me.is_none() || self.holder() != me,
            "sleeplock: {} already holds this lock",
            OwnerName(word_of(me)),
        );
        let owner = word_of(me);
        if let Some(guard) = self.take(owner) {
            return guard;
        }
        // Queued: from here the release owes this ticket a post, and
        // `wait_uncancellable_until` arms before it re-reads `now` — which is
        // what stops a release landing in between from being lost.
        let mine = self.ticket.fetch_add(1, Ordering::Relaxed);
        completion::wait_uncancellable_until(
            p,
            Subject::of(&self.watch),
            Token::new(u64::from(mine)),
            || self.now.load(TURN) == mine,
        );
        self.holder.store(owner, Ordering::Relaxed);
        SleepGuard { lock: self }
    }

    /// Take it if it is free, from any context — an interrupt handler, the
    /// panic path, boot.
    ///
    /// Total, and it takes no [`Parkable`]: that is what makes it the answer for
    /// a caller that already holds a [`crate::sync::Lock`], and the one
    /// filesystem door boot has (§6.1).
    pub fn try_lock(&self) -> Option<SleepGuard<'_, T>> {
        self.take(word_of(current_task()))
    }

    /// The uncontended acquire: one compare-exchange, exactly as
    /// `Lock::try_lock` does it, plus the holder store.
    ///
    /// It fails whenever anybody is queued, because the ticket has then already
    /// moved past `now` — which is what stops a `try_lock` jumping the queue.
    fn take(&self, owner: u64) -> Option<SleepGuard<'_, T>> {
        let turn = self.now.load(TURN);
        self.ticket
            .compare_exchange(
                turn,
                turn.wrapping_add(1),
                Ordering::Relaxed,
                Ordering::Relaxed,
            )
            .ok()?;
        self.holder.store(owner, Ordering::Relaxed);
        Some(SleepGuard { lock: self })
    }

    /// Who holds it.
    ///
    /// **`None` is not "free".** It is "not a task", which is what a lock nobody
    /// holds and a lock boot took through [`SleepLock::try_lock`] both look
    /// like: the answer exists for a reader that wants to name a *thread*, and
    /// neither of those is one. §17.1 is where this is priced — it buys
    /// `sched::dump` nothing, because the dump reaches none of §9's four locks,
    /// and it exists for the self-deadlock refusal in [`SleepLock::lock`].
    ///
    /// One relaxed load, and it may be taken from any context including the
    /// panic path.
    pub fn holder(&self) -> Option<TaskId> {
        match self.holder.load(Ordering::Relaxed) {
            FREE | NOT_A_TASK => None,
            word => Some(TaskId::unpack(word)),
        }
    }
}

/// A packed owner word for a refusal message, printed as a task where it names
/// one. Its own type so the `no task` case reads as a sentence rather than as
/// `4294967295:4294967294`.
struct OwnerName(u64);

impl core::fmt::Display for OwnerName {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.0 {
            FREE | NOT_A_TASK => write!(f, "a context with no task"),
            word => write!(f, "{}", TaskId::unpack(word)),
        }
    }
}

pub struct SleepGuard<'a, T> {
    lock: &'a SleepLock<T>,
}

impl<T> Deref for SleepGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: the ticket pair gave this caller the lock and has not yet
        // published the release below.
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for SleepGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as `deref`, and `&mut self` is the exclusive borrow.
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for SleepGuard<'_, T> {
    /// **The holder word first, and the order is load-bearing.** After the
    /// release below another CPU may already own this lock and have written its
    /// own identity, so a store here would name the wrong task — and
    /// [`SleepLock::holder`] would answer with a thread that does not hold it,
    /// which is the one question this word exists to answer.
    ///
    /// The post is [`completion::post_n`] with a limit of one and the next
    /// ticket as the token, so it wakes the contender whose turn it now is and
    /// nobody else. On an uncontended lock it costs one relaxed load and
    /// returns.
    fn drop(&mut self) {
        self.lock.holder.store(FREE, Ordering::Relaxed);
        let next = self
            .lock
            .now
            .fetch_add(1, Ordering::Release)
            .wrapping_add(1);
        let _ = completion::post_n(
            Subject::of(&self.lock.watch),
            Outcome::Ready,
            Token::new(u64::from(next)),
            1,
        );
    }
}
