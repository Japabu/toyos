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

use toyos_sched::hw::Nanos;

use toyos_sched::task::WaitClass;

use crate::sched::payload::{KShared, TaskHandle};
use crate::scheduler::Parkable;
use crate::sync::Lock;
use crate::time::Deadline;

pub use toyos_sched::waitq::Cancel;

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
    /// raw pointer would make an abandoned arm a use-after-free instead of a
    /// bounded leak. (Since §7.2 an abandoned arm is itself rare — a killed
    /// task runs its own unwind and drops it — but the `Arc` is what makes
    /// "rare" not have to be "never".)
    task: Arc<TaskHandle>,
    /// The waiter's rendezvous word, for the claim half of the post. Held
    /// beside the handle because the two are minted at different instants and
    /// a post needs both: the record, then the claim.
    shared: Arc<KShared>,
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
    shared: Arc<KShared>,
    token: Token,
    /// What this wait is, for the blocked-time breakdown. Decided here rather
    /// than at the park, because the park is on this thread's own queue and
    /// that queue has no subject to read a class off — see
    /// `toyos_sched::waitq::WaitQueue::prepare_wait_as`.
    class: WaitClass,
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
        inbox.disarm();
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
/// `class` is what this wait's blocked time is attributed to. It belongs to the
/// arm because it is a property of the *subject*: a thread parked on a pipe end
/// is blocked on a pipe however it got there, and the queue it physically parks
/// on is its own and says nothing.
///
/// `None` when there is no current task: boot has none, and neither has an
/// idle CPU.
pub fn arm(subject: Subject<'_>, token: Token, class: WaitClass) -> Option<Armed<'_>> {
    let task = crate::sched::driver::current_handle()?;
    let shared = crate::sched::driver::current_shared()?;
    let inbox = task.inbox();
    assert!(
        !inbox.is_armed(),
        "completion::arm: this task is already armed on a subject",
    );
    inbox.arm_to(token);
    let watch = subject.0;
    let mut waiters = watch.waiters.lock();
    waiters.push(Watcher {
        task: task.clone(),
        shared: shared.clone(),
        token,
    });
    watch.armed.store(waiters.len(), Ordering::Relaxed);
    drop(waiters);
    Some(Armed { subject, task, shared, token, class })
}

/// Tell everyone armed on `subject` that something happened.
///
/// Callable from an interrupt handler and from inside a lock: it takes one leaf
/// lock and stores, exactly as the watcher-list walk it sits beside already
/// does.
pub fn post(subject: Subject<'_>, outcome: Outcome) {
    post_with(subject, outcome, None)
}

/// The same, lending the poster's real-time window to whoever it wakes.
///
/// **Priority inheritance survives the conversion**, and it had to be carried
/// deliberately: the queue wake this replaces took a `WakeCause::boosted`, and
/// a completion post that dropped it would silently turn an RT writer's signal
/// into an ordinary one — scheduler-core-spec §3's lend, invariant I9, and the
/// audio path's whole latency argument.
pub fn post_boosted(subject: Subject<'_>, outcome: Outcome, until: Nanos) {
    post_with(subject, outcome, Some(until))
}

/// Tell at most `limit` of the waiters armed on `subject` **for this token**
/// that something happened, and answer how many were told.
///
/// **The counted form, and the token is what makes counting mean anything.**
/// `SYS_FUTEX_WAKE`'s ABI is "wake up to `count` threads waiting on `addr`,
/// return the number woken", and a subject whose waiters are a *hash bucket*
/// cannot honour either half: the bucket holds waiters of every word that
/// hashes into it, so a count-limited walk over the bucket would spend the
/// caller's single wake on a thread waiting for a different word and leave the
/// intended one parked. A shared queue's spurious wake is harmless because
/// every waiter re-checks; a shared queue's *stolen* wake is not.
///
/// The token closes it without a second channel, which §23's rejection 3
/// forbids: a futex waiter arms with its word's physical address as its token,
/// so this walk names the word rather than the bucket. A waiter of another word
/// in the same bucket is skipped and does not count against `limit`.
///
/// A `limit` of zero tells nobody, which is what a caller asking for zero
/// wakes means; `usize::MAX` is the broadcast every `pthread_cond_broadcast`
/// asks for.
pub fn post_n(subject: Subject<'_>, outcome: Outcome, token: Token, limit: usize) -> usize {
    let watch = subject.0;
    if limit == 0 || watch.armed.load(Ordering::Relaxed) == 0 {
        return 0;
    }
    let at = crate::clock::now();
    let waiters = watch.waiters.lock();
    let mut told = 0;
    for waiter in waiters.iter() {
        if told == limit {
            break;
        }
        if waiter.token != token {
            continue;
        }
        post_to(waiter, outcome, at, None);
        told += 1;
    }
    told
}

fn post_with(subject: Subject<'_>, outcome: Outcome, boost: Option<Nanos>) {
    let watch = subject.0;
    // The whole cost on a subject nobody waits on. Read before the lock, so a
    // wake that would otherwise be two stores does not become a lock acquire.
    if watch.armed.load(Ordering::Relaxed) == 0 {
        return;
    }
    let at = crate::clock::now();
    let waiters = watch.waiters.lock();
    for waiter in waiters.iter() {
        post_to(waiter, outcome, at, boost);
    }
}

/// **Invariant W, in two statements** (§5.4): the record first, under this
/// subject's leaf lock, and then the claim. A parker that has published
/// `Committing` is refused its park by the claim; one that has not yet
/// re-checked finds the record; one already `Blocked` gets the message. There
/// is no fourth case, which is what `kernel-loom/tests/inbox.rs` is about.
fn post_to(waiter: &Watcher, outcome: Outcome, at: crate::time::Instant, boost: Option<Nanos>) {
    waiter.task.inbox().post(Record {
        token: waiter.token,
        outcome,
        at,
    });
    crate::scheduler::wake_sched(&waiter.shared, boost);
}

/// The right to park, and the answer a killed thread gets instead.
///
/// A zero-sized type kernel code cannot construct: the only way to hold one is
/// to have been told, by the one `wait` that reports it, that this thread has
/// been killed. That is what stops a caller manufacturing a cancel, and RT4 —
/// the second cancel reported to one thread panics — is what stops one being
/// swallowed.
#[derive(Debug)]
pub struct Cancelled(());

/// Park until a record arrives, the deadline passes, or this thread is
/// cancelled.
///
/// **The one park site in the kernel.** Every blocking syscall reaches the
/// machine through here, and the whole of its recheck is
/// [`Inbox::has_record`] — one predicate, with no source named in it, which is
/// what makes a new wait source unable to re-open the lost-wake window.
///
/// The arm is taken by reference and outlives the call, which is §5.3a's edge
/// contract rather than §5.3's signature: a caller loops, re-deriving its own
/// predicate between waits, and a post landing in that window must find the
/// watch still armed. An arm consumed per wait would lose exactly the wake
/// that arm-before-check exists to catch.
///
/// A deadline that passes is an [`Outcome::Gone`] with [`Reason::Expired`] and
/// not an error: the caller asked for it, and §3's `Deadline` is the kind that
/// says whose business the expiry is.
#[track_caller]
pub fn wait(p: &Parkable, armed: &Armed<'_>, deadline: Deadline) -> Result<Record, Cancelled> {
    wait_inner(p, armed, deadline, Cancel::Answers)
}

/// The same, for a wait a kill may not end.
///
/// §7.4's third shape. One caller — the retirer waiting for its victim's
/// release — and its bound is its own tripwire, never the kill: a killed
/// retirer that took `Cancelled` here could not propagate it (the retire is
/// half done) and would spin on a commit that refuses to park.
#[track_caller]
pub fn wait_uncancellable(p: &Parkable, armed: &Armed<'_>, deadline: Deadline) -> Record {
    match wait_inner(p, armed, deadline, Cancel::Ignores) {
        Ok(record) => record,
        Err(_) => unreachable!("an uncancellable wait never reports a cancel"),
    }
}

/// Arm, then park until `ready()` holds, the deadline passes, or this thread is
/// cancelled.
///
/// **The shape every blocking syscall in the kernel now has**, and the direct
/// replacement for `scheduler::wait_until`. The arm comes first and the
/// predicate is re-derived after it — §5.3a's edge contract — so a post that
/// lands in the window between the two is found by the park's own recheck
/// rather than lost.
///
/// A return is not proof of the condition (scheduler-core-spec §2's invariant
/// 10): the loop is what holds the wait until the predicate is true, and a
/// deadline that passes returns with it still false, which is what the one
/// timed caller needs.
#[track_caller]
pub fn wait_until(
    p: &Parkable,
    subject: Subject<'_>,
    token: Token,
    class: WaitClass,
    deadline: Deadline,
    ready: impl Fn() -> bool,
) -> Result<(), Cancelled> {
    if ready() {
        return Ok(());
    }
    let Some(armed) = arm(subject, token, class) else {
        // No current task: boot, or an idle CPU. Neither can park, and neither
        // reaches a blocking syscall — this is the `Parkable` argument stated
        // once more at runtime, for the one caller that could be reached from
        // a kernel thread before the scheduler exists.
        return Ok(());
    };
    loop {
        if ready() {
            return Ok(());
        }
        let record = wait(p, &armed, deadline)?;
        if record.outcome == Outcome::Gone(Reason::Expired) {
            return Ok(());
        }
    }
}

/// A kernel thread that has said everything it has to say.
///
/// **Armed on itself, where nothing posts.** The two log actuator threads park
/// here rather than exiting, because a thread that exits frees a stack a
/// producer may still be about to write to; what they must not do is spin,
/// which is what they would be doing if they competed with the reader for the
/// rest of the boot. This is what took the last two callers off
/// `scheduler::park_lot`, which is deleted with them.
#[cfg(feature = "boot-actuators")]
#[track_caller]
pub fn park_forever() -> ! {
    let parkable = crate::scheduler::Parkable::of_current();
    let handle = crate::sched::driver::current_handle().expect("a kernel thread is a task");
    let armed =
        arm(Subject::of(handle.watch()), Token::new(0), WaitClass::Other).expect("a task can arm");
    loop {
        let _ = wait(&parkable, &armed, Deadline::never());
    }
}

#[track_caller]
fn wait_inner(
    _p: &Parkable,
    armed: &Armed<'_>,
    deadline: Deadline,
    cancel: Cancel,
) -> Result<Record, Cancelled> {
    let task = &armed.task;
    loop {
        if let Some(record) = task.inbox().take() {
            return Ok(record);
        }
        if cancel == Cancel::Answers && task.take_cancel(armed.shared.kill_pending()) {
            return Err(Cancelled(()));
        }
        if deadline.reached(crate::clock::now()) {
            return Ok(Record {
                token: armed.token,
                outcome: Outcome::Gone(Reason::Expired),
                at: crate::clock::now(),
            });
        }
        // Register on this thread's own parking place, re-check, park. The
        // registration precedes the re-check, which is the whole of §2's
        // invariant 4; the queue is never woken as a queue, because a post
        // claims the rendezvous word directly.
        let ticket = crate::scheduler::prepare_wait(task.park_queue(), cancel, armed.class);
        if task.inbox().has_record()
            || (cancel == Cancel::Answers && armed.shared.kill_pending())
        {
            ticket.cancel();
            continue;
        }
        crate::scheduler::block_on(ticket, deadline);
    }
}
