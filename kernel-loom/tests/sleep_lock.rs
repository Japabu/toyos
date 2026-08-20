//! Loom: `SleepLock`'s acquire edges, and the first *contended* acquire this
//! tree has ever modelled.
//!
//! `Lock::lock`'s spin is unreachable by loom — the model checker explores a
//! `core::hint::spin_loop` as an unbounded branch and gives up, which is what
//! `issues/kernel/lock-spin-unreachable-by-loom.md` records, so
//! `ticket_lock.rs` can only drive `try_lock`. A parking acquire has no
//! unbounded branch: the contender gives the CPU back and loom's own
//! `yield_now` is a branch it can bound. So the path `ticket_lock.rs` cannot
//! reach is exactly the path this file is about.
//!
//! **What this proves and what it does not.** Loom has no scheduler, so the
//! park itself is shimmed
//! (`kernel_loom::completion::wait_uncancellable_until` yields). What is
//! therefore under test is the lock: the ticket arithmetic, mutual exclusion,
//! and the release-to-acquire edge that hands the next holder the previous
//! one's writes. What is **not** under test is the wake handshake — the
//! record-then-claim pair that makes a post reach a *parked* waiter, which is
//! `inbox.rs`'s model and is checked there against the real `Inbox`.
//!
//! Service order is FIFO by the arithmetic rather than by anything this file
//! observes: `now` is advanced exactly once per release and a contender waits
//! for the ticket it took, so ticket order *is* service order. There is no
//! interleaving that makes that false and none that makes it visible, so it is
//! stated here rather than asserted below.
//!
//! On x86 every load is an acquire, so a build with the edge relaxed behaves
//! identically to this one and no guest test can fail here. The negative case
//! is a cargo feature rather than a comment:
//!
//! ```text
//! cargo test --manifest-path kernel-loom/Cargo.toml --features sleeplock-acquire-off \
//!   --test sleep_lock
//! ```
//!
//! makes `sleeplock.rs`'s two loads of `now` relaxed and this file must red —
//! loom answers `Causality violation: Concurrent write accesses to UnsafeCell`,
//! which is a lock handing out data it did not synchronize, stated exactly.
//! Verified 2026-08-19, both ways round: all four models red under the feature
//! and all four pass without it. The step that runs it is `host-tests.yml`'s
//! "kernel-loom sleep lock's acquire edge has teeth", which demands
//! `a_parking_contender_observes_the_holders_writes` and
//! `two_holders_never_overlap` by name — the contended park and mutual
//! exclusion, the two the *ordering* breaks rather than the arithmetic.

use kernel_loom::scheduler::{become_task, Parkable, TaskId};
use kernel_loom::sleeplock::SleepLock;
use loom::sync::Arc;

/// The two identities the models use. Distinct, because the holder word and the
/// self-deadlock refusal are about *which* task holds the lock.
const ONE: TaskId = TaskId(1, 1);
const TWO: TaskId = TaskId(1, 2);

/// A `try_lock` that succeeds must see every write the previous holder made.
///
/// The mirror of `ticket_lock.rs`'s first model, on the type that replaces it
/// where the holder may be descheduled. Neither thread parks, so what this one
/// isolates is the edge alone.
#[test]
fn try_lock_observes_the_previous_holders_writes() {
    loom::model(|| {
        let lock = Arc::new(SleepLock::new(0u32));

        let writer = {
            let lock = lock.clone();
            loom::thread::spawn(move || {
                become_task(TWO);
                if let Some(mut guard) = lock.try_lock() {
                    *guard = 42;
                }
            })
        };

        become_task(ONE);
        if let Some(guard) = lock.try_lock() {
            let seen = *guard;
            assert!(
                seen == 0 || seen == 42,
                "try_lock handed out a value nobody wrote: {seen}",
            );
        }

        writer.join().unwrap();
    });
}

/// **The model that did not exist before this type did.** A contender that had
/// to queue — take a ticket, arm, give the CPU back — must see the holder's
/// writes when its turn comes.
///
/// Both threads acquire with `lock`, so on every schedule where they collide
/// one of them goes down the queued path and the release edge is what hands it
/// the data. This is the model `sleeplock-acquire-off` reds.
#[test]
fn a_parking_contender_observes_the_holders_writes() {
    loom::model(|| {
        let lock = Arc::new(SleepLock::new(0u32));

        let other = {
            let lock = lock.clone();
            loom::thread::spawn(move || {
                become_task(TWO);
                let parkable = Parkable::at_entry();
                let mut guard = lock.lock(&parkable);
                // Read-modify-write *through the guard*: a lock that failed to
                // synchronize hands this side a stale value, which is the
                // defect, and loom sees the unordered pair either way.
                *guard += 1;
            })
        };

        become_task(ONE);
        let parkable = Parkable::at_entry();
        {
            let mut guard = lock.lock(&parkable);
            *guard += 1;
        }

        other.join().unwrap();

        let guard = lock
            .try_lock()
            .expect("both holders released, so the lock is free");
        assert_eq!(*guard, 2, "a holder's increment went missing");
    });
}

/// Two acquires never overlap, however they got in.
///
/// The counter above would survive an overlap on a machine where the two
/// increments happened to interleave harmlessly; this one cannot. Each holder
/// claims the slot on entry and gives it back on exit, so a second holder
/// arriving while the first is inside finds it taken.
#[test]
fn two_holders_never_overlap() {
    loom::model(|| {
        let lock = Arc::new(SleepLock::new(false));

        let other = {
            let lock = lock.clone();
            loom::thread::spawn(move || {
                become_task(TWO);
                let parkable = Parkable::at_entry();
                let mut guard = lock.lock(&parkable);
                assert!(!*guard, "two tasks held the lock at once");
                *guard = true;
                *guard = false;
            })
        };

        become_task(ONE);
        let parkable = Parkable::at_entry();
        {
            let mut guard = lock.lock(&parkable);
            assert!(!*guard, "two tasks held the lock at once");
            *guard = true;
            *guard = false;
        }

        other.join().unwrap();
    });
}

/// A contender that queued is always served, and `holder` names it while it is
/// inside.
///
/// Termination is half the claim — every schedule loom explores runs to the end,
/// so no ticket is ever left unserved — and the other half is that the word the
/// self-deadlock refusal reads agrees with who is actually holding it.
#[test]
fn a_queued_contender_is_served_and_named() {
    loom::model(|| {
        let lock = Arc::new(SleepLock::new(0u32));

        let other = {
            let lock = lock.clone();
            loom::thread::spawn(move || {
                become_task(TWO);
                let parkable = Parkable::at_entry();
                let _guard = lock.lock(&parkable);
                assert_eq!(lock.holder(), Some(TWO), "the holder word names nobody");
            })
        };

        become_task(ONE);
        let parkable = Parkable::at_entry();
        {
            let _guard = lock.lock(&parkable);
            assert_eq!(lock.holder(), Some(ONE), "the holder word names nobody");
        }

        other.join().unwrap();
        assert_eq!(lock.holder(), None, "a free lock still names a holder");
    });
}
