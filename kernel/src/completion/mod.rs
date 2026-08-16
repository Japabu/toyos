//! One completion primitive: a record, an inbox, and a watch a waiter lends to
//! the object it is waiting on.
//!
//! `specs/completion-architecture-spec.md` §5. The claim the whole design rests
//! on is that **every wait in this kernel is "a record in an inbox"** — so the
//! park-time recheck is one predicate with no source named in it, and a new
//! wait source cannot re-open the lost-wake window because it has no way to add
//! a second predicate.
//!
//! **What C2 lands, and what it deliberately does not.** The core is wired
//! *behind* the existing wait queues: a park still registers a
//! `toyos_sched` ticket and is still woken by a queue wake, and what is new is
//! that the waiter also arms an inbox on the subject and every wake of that
//! subject also posts a record. Nothing parks on the inbox alone yet — C3 is
//! the chunk that makes [`Inbox::has_record`] the park predicate and deletes
//! the queue half — so this chunk is behaviour-preserving by construction, and
//! what it buys is that the post path, the record's publication and the arm's
//! bookkeeping are all live and under test before anything depends on them.
//!
//! **Which subjects exist here.** The four device queues
//! (`waitqs::{KEYBOARD, MOUSE, NETWORK, AUDIO}`) and both ends of a pipe: five
//! park sites — a pipe read, a pipe write, a console read, and the two audio
//! period reads — against the sites that wake them. C3 adds the rest with the
//! park conversion that needs them: the port acceptor, the process and thread
//! objects, the io_uring ring, the futex bucket and the CPU's deadline list.
//!
//! **No registry, and no id.** A [`Subject`] is a borrowed reference to the
//! object being waited on, so a destroyed subject cannot be named and §5.1's
//! "the completion core maps no id to any object" holds structurally. A post is
//! a walk of one object's own list under its own leaf lock, which is the shape
//! `sched::waitqs` already has and which deletes the 128-core sharding risk a
//! global `CORE` lock would have had.
//!
//! **The cost, stated because §16.2 requires it to be counted rather than
//! asserted.** A post to a subject nobody is armed on is *one relaxed load* and
//! no lock at all — the same trick the log's `signal_after_commit` uses, and
//! the reason the record path has no read-modify-write on it. A post that finds
//! a waiter costs one `Lock` acquire plus a plain store per waiter. An arm
//! costs one `Arc` clone and one `Lock` acquire; a disarm the same. Nothing on
//! the wake path gained a read-modify-write it did not have.
//!
//! **The hazard this chunk inherits and does not fix.** A task killed while
//! parked never drops its [`Armed`], because this kernel does not unwind — so
//! its node stays on the watch list and the `Arc<TaskHandle>` behind it leaks,
//! bounded and census-visible. That is the endowment spec's §1.1 leak class and
//! §7's subject; C3+C4 closes it by making a killed task run its own unwind.
//! It is memory, never unsoundness: the watch holds an `Arc`, so nothing here
//! can point at a freed inbox.

pub mod inbox;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

pub use inbox::{Inbox, Outcome, Reason, Record, Token};

use crate::sched::payload::TaskHandle;
use crate::sync::Lock;

/// The waiters armed on one object.
///
/// Every waitable object owns one, beside the wait queue it already owns. The
/// count is the whole of what a post pays when nobody is waiting: a relaxed
/// load, read *before* the lock, so an idle machine's wakes cost exactly what
/// they cost today.
pub struct Watch {
    armed: AtomicUsize,
    waiters: Lock<Vec<Watcher>>,
}

struct Watcher {
    /// The waiter's own inbox, held by `Arc` rather than by reference: a
    /// killed waiter never drops its `Armed` (§7), and a raw pointer would
    /// make that a use-after-free instead of a bounded leak.
    task: Arc<TaskHandle>,
    token: Token,
}

impl Watch {
    pub const fn new() -> Self {
        Self {
            armed: AtomicUsize::new(0),
            waiters: Lock::new(Vec::new()),
        }
    }
}

/// What is being waited on. **A reference, never an id.**
#[derive(Clone, Copy)]
pub struct Subject<'a>(&'a Watch);

impl<'a> Subject<'a> {
    pub const fn of(watch: &'a Watch) -> Self {
        Self(watch)
    }
}

/// Proof that a record will arrive on the armed inbox for this token.
///
/// `#[must_use]` and not `Copy`; `Drop` disarms. A park with nothing armed is
/// untypeable once C3 makes the park take one of these (RT3).
#[must_use = "an arm must outlive the park it was made for"]
pub struct Armed<'a> {
    subject: Subject<'a>,
    task: Arc<TaskHandle>,
    token: Token,
}

impl Drop for Armed<'_> {
    /// Take the node off the list, then drain what arrived — **and check the
    /// invariant the inbox's plain stores rest on while draining it.**
    ///
    /// One arm at a time means every record in this inbox was posted by the
    /// subject this arm named, so it carries this arm's token. A record with
    /// another token is two posters on one inbox, which is the one way the
    /// lock-free `tail` store could be wrong — and it would otherwise be
    /// invisible until C3 makes something read these records. The overflow
    /// notice is the exception by construction: it is minted by the taker and
    /// names no subject.
    fn drop(&mut self) {
        let watch = self.subject.0;
        let mut waiters = watch.waiters.lock();
        if let Some(at) = waiters.iter().position(|w| Arc::ptr_eq(&w.task, &self.task)) {
            waiters.swap_remove(at);
        }
        watch.armed.store(waiters.len(), Ordering::Relaxed);
        drop(waiters);
        let inbox = self.task.inbox();
        inbox.set_armed(false);
        while let Some(record) = inbox.take() {
            assert!(
                record.token == self.token
                    || record.outcome == Outcome::Gone(Reason::Overflowed),
                "completion: a record posted at {} reached an inbox armed on another subject",
                record.at.nanos_since_boot(),
            );
        }
    }
}

/// Arm a watch for the running task.
///
/// **This is the edge form**, and C2 has no other: the record a post leaves
/// means "state may have moved", never "there is something for you", so the
/// waiter's own predicate stays authoritative and is re-derived after this
/// returns — which is what every site here does today anyway, through
/// `wait_until`'s loop. §5.3's level form, where `arm` asks the subject and
/// fires immediately, arrives with C3's park conversion and the readiness
/// question that goes with it.
///
/// `None` when there is no current task: boot has none, and neither has an
/// idle CPU.
pub fn arm(subject: Subject<'_>, token: Token) -> Option<Armed<'_>> {
    let task = crate::sched::driver::current_handle()?;
    let inbox = task.inbox();
    assert!(
        !inbox.is_armed(),
        "completion::arm: this task is already armed on a subject",
    );
    // A new wait starts owing nothing. Whatever the last one was told is the
    // last one's business, and it has already returned.
    inbox.reset();
    inbox.set_armed(true);
    let watch = subject.0;
    let mut waiters = watch.waiters.lock();
    waiters.push(Watcher { task: task.clone(), token });
    watch.armed.store(waiters.len(), Ordering::Relaxed);
    drop(waiters);
    Some(Armed { subject, task, token })
}

/// Tell everyone armed on `subject` that something happened.
///
/// Callable from an interrupt handler and from inside a lock: it takes one leaf
/// lock and stores, exactly as the watcher-list walk it sits beside already
/// does.
pub fn post(subject: Subject<'_>, outcome: Outcome) {
    let watch = subject.0;
    // The whole cost on a subject nobody waits on. Read before the lock, so a
    // wake that would otherwise be two stores does not become a lock acquire.
    if watch.armed.load(Ordering::Relaxed) == 0 {
        return;
    }
    let at = crate::clock::now();
    let waiters = watch.waiters.lock();
    for waiter in waiters.iter() {
        waiter.task.inbox().post(Record {
            token: waiter.token,
            outcome,
            at,
        });
    }
}
