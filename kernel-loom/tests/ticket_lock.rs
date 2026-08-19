//! Loom: `Lock`'s acquire edges.
//!
//! `try_lock` decides ownership by CASing `ticket`, but the atomic an unlock
//! publishes through is `now`. Whichever operation reads `now` is therefore the
//! one that has to carry the acquire — an acquire on `ticket` synchronizes with
//! nothing, because nothing ever releases to `ticket`.
//!
//! On x86 every load is an acquire, so a build with that edge relaxed behaves
//! identically to this one and no guest test can fail here. The negative case is
//! a cargo feature rather than a comment:
//!
//! ```text
//! cargo test --manifest-path kernel-loom/Cargo.toml --features lock-acquire-off \
//!   --test ticket_lock
//! ```
//!
//! makes `sync.rs`'s two loads of `now` relaxed and this file must red — loom
//! answers `Causality violation: Concurrent write accesses to UnsafeCell`, which
//! is a lock handing out data it did not synchronize, stated exactly. Verified
//! 2026-08-17, both ways round.

use kernel_loom::sync::Lock;
use loom::sync::Arc;

/// A `try_lock` that succeeds must see every write the previous owner made.
///
/// Both threads acquire through `try_lock`, so nothing spins. The release edge
/// under test is `LockGuard::drop`, which is the same one whichever path the
/// previous owner acquired by.
#[test]
fn try_lock_observes_the_previous_owners_writes() {
    loom::model(|| {
        let lock = Arc::new(Lock::new(0u32));

        let writer = {
            let lock = lock.clone();
            loom::thread::spawn(move || {
                if let Some(mut guard) = lock.try_lock() {
                    *guard = 42;
                }
            })
        };

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

/// Two `try_lock`s never hold the lock at once, and the loser leaves the ticket
/// where it found it — so the winner's own unlock still frees the lock.
#[test]
fn two_try_locks_do_not_both_succeed() {
    loom::model(|| {
        let lock = Arc::new(Lock::new(0u32));

        let contender = {
            let lock = lock.clone();
            loom::thread::spawn(move || match lock.try_lock() {
                Some(mut guard) => {
                    *guard += 1;
                    true
                }
                None => false,
            })
        };

        let mine = match lock.try_lock() {
            Some(mut guard) => {
                *guard += 1;
                true
            }
            None => false,
        };

        let theirs = contender.join().unwrap();
        assert!(mine || theirs, "an uncontended lock refused both callers");

        let guard = lock
            .try_lock()
            .expect("both holders are gone, so the lock is free");
        assert_eq!(
            *guard,
            u32::from(mine) + u32::from(theirs),
            "a holder's increment went missing",
        );
    });
}
