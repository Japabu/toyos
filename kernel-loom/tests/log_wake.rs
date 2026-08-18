//! Loom: **W3 — the wake edge**, and the one obligation the wake's own
//! correctness rests on.
//!
//! The invariant, in one sentence: **after a commit and an arm, at least one
//! side has seen the other.** Either the producer finds the flag and owns a
//! post, or the reader's rescan finds the record and does not park. Never
//! neither — that is a committed record left under a parked reader, which is a
//! console that has gone quiet with something still to say.
//!
//! Both halves are a store followed by a load of a *different* location, which
//! is the one reordering x86 TSO permits — so on the machine this kernel ships
//! for, a build with either `SeqCst` fence removed behaves identically to one
//! with both. **No guest test can fail here, on any hardware this tree
//! targets**, which is why the obligation is a model and why the model ships in
//! the same chunk as the code.
//!
//! **What this model is not about.** The park itself is `toyos-sched`'s
//! two-phase commit: `klogd` registers *before* it arms, so a producer that
//! wins the flag from that point on claims a `Committing` task and the task's
//! own commit refuses to park. That handshake has its own models in that crate,
//! and re-modelling it here would only re-derive it badly — the first draft of
//! this file did, with a plain `parked` word, and reported a lost wake that the
//! rendezvous CAS makes unreachable. What is left here is exactly the pair of
//! fences, which nothing else models.
//!
//! The negative case is a cargo feature rather than a comment:
//!
//! ```text
//! cargo test --manifest-path kernel-loom/Cargo.toml --features wake-fence-off \
//!   --test log_wake
//! ```
//!
//! removes both fences from `shard.rs` and this file must red. A model that has
//! never failed proves nothing.
//!
//! `specs/log-architecture-spec.md` §2.5's W3 and §2.6a.

#![cfg(feature = "loom")]

use kernel_loom::arch::LogCommitGuard;
use kernel_loom::log_shard::{arm_waiter, signal_after_commit, waiter, Shard, FIRST_SEQ};
use loom::sync::atomic::{AtomicBool, Ordering};
use loom::sync::Arc;
use toyos_abi::log::LogRecord;

struct Machine {
    shard: Shard,
    waiter: AtomicBool,
    /// Set by the producer when `signal_after_commit` says it owns the post.
    /// The post itself is `wake_direct`'s claim CAS and is not this model's
    /// subject; that it was *reached* is.
    posted: AtomicBool,
}

fn record(seq: u64) -> LogRecord {
    LogRecord { seq, at_ns: seq, len: 0, ..LogRecord::EMPTY }
}

/// **The invariant.** One producer commits one record; one reader arms and
/// re-scans. Whatever the schedule, the run may not end with the producer
/// having posted nothing *and* the reader having decided to park.
///
/// Remove either fence and it can: the producer's flag load is allowed to move
/// ahead of its commit store and the reader's rescan ahead of its flag store,
/// so both sides look at the other's previous value.
#[test]
fn a_commit_and_an_arm_cannot_both_miss() {
    loom::model(|| {
        let m = Arc::new(Machine {
            shard: Shard::new(),
            waiter: waiter(),
            posted: AtomicBool::new(false),
        });

        let producer = m.clone();
        let p = loom::thread::spawn(move || {
            let guard = LogCommitGuard::close();
            // SAFETY: one producer, and the guard is the model's stand-in for
            // the kernel's sole-writer bracket (`percpu_fetch_add`'s shim
            // argument in `lib.rs` is the other half).
            let seq = unsafe { producer.shard.reserve(&guard) };
            unsafe { producer.shard.commit(seq, &record(seq), &guard) };
            drop(guard);
            if signal_after_commit(&producer.waiter) {
                producer.posted.store(true, Ordering::SeqCst);
            }
        });

        // `klogd`'s park decision, and nothing else of its body: `true` is "do
        // not park".
        let dont_park = arm_waiter(&m.waiter, || m.shard.at_ns(FIRST_SEQ).is_some());

        p.join().unwrap();

        assert!(
            dont_park || m.posted.load(Ordering::SeqCst),
            "the producer posted nothing and the reader decided to park: a committed record \
             is left with a parked reader, and only a missing fence can produce it"
        );
    });
}

/// Two producers, one flag: exactly one of them owns the post.
///
/// This is what the `swap` in `signal_after_commit` buys, and it is why the
/// flag is *loaded* first — a producer that finds it clear pays no
/// read-modify-write at all, which is the whole reason the record path has none
/// on it (`issues/hardware/one-rmw-per-log-line-cost-350ms.md`).
#[test]
fn exactly_one_producer_owns_a_park() {
    loom::model(|| {
        let flag = Arc::new(waiter());
        flag.store(true, Ordering::SeqCst);

        let a = flag.clone();
        let ta = loom::thread::spawn(move || u32::from(signal_after_commit(&a)));
        let owned_b = u32::from(signal_after_commit(&flag));
        let owned_a = ta.join().unwrap();

        assert_eq!(
            owned_a + owned_b,
            1,
            "the swap admits one poster per park: {owned_a} + {owned_b}"
        );
    });
}
