//! Loom: **the record's publication, and Invariant W** — the two things a
//! completion has to get right and no guest test can see.
//!
//! The invariant, in one sentence: **after a post and a park decision, the
//! record is either in the taker's hands or the poster owns the wake.** Never
//! neither, which is a completion left under a parked task — a thread that
//! waits for something that already happened.
//!
//! Both halves are a store followed by a load of a *different* location, the
//! one reordering x86 TSO permits, so on the machine this kernel ships for a
//! build with the release removed behaves identically to one with it. **No
//! guest test can fail here, on any hardware this tree targets**, which is why
//! the obligation is a model and why the model ships in the same chunk as the
//! code. The negative case is a cargo feature rather than a comment:
//!
//! ```text
//! cargo test --manifest-path kernel-loom/Cargo.toml --features inbox-release-off \
//!   --test inbox
//! ```
//!
//! makes the publication relaxed and this file must red — loom answers
//! `Causality violation: Concurrent write accesses to UnsafeCell`, which is the
//! defect stated exactly. Verified 2026-08-16, on this branch, both ways round.
//!
//! **Invariant W's own teeth are a mutation rather than a feature**, because
//! what would have to be broken is the *caller's* order and not a line in
//! `inbox.rs`: move the recheck above the registration in
//! `a_post_racing_a_park_leaves_nobody_waiting` — check-then-block, the window
//! the two-phase commit exists to close — and the model reds. Also verified
//! 2026-08-16.
//!
//! **What this model is not about.** The rendezvous that claims a waiter is
//! `toyos-sched`'s two-phase commit, and it has its own models in that crate;
//! re-deriving it here would only re-derive it badly. What stands in for it
//! below is a `SeqCst` flag, which is what the real `compare_exchange` gives
//! and no more. The subject's leaf lock is a `loom::sync::Mutex` for the same
//! reason: the kernel's poster really does hold one, and the inbox's plain
//! `tail` store is sound *because* of it.
//!
//! **Both sides are spawned threads on purpose.** Loom runs the model's own
//! thread first and explores preemptions from there, so a consumer written on
//! the main thread never observes a producer that has not run yet: the
//! interleaving that matters is not in the state space at all, and the model
//! passes vacuously. Measured while writing this file — the reader-on-main
//! form asserted `!has_record()` and *passed*, with the release in place.
//!
//! `specs/completion-architecture-spec.md` §5.4 and §16.1.

#![cfg(feature = "loom")]

use kernel_loom::inbox::{Inbox, Outcome, Record, Token};
use kernel_loom::time::Instant;
use loom::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use loom::sync::{Arc, Mutex};

const TOKEN: Token = Token::new(0xC0FFEE);

fn record(at: u64) -> Record {
    Record {
        token: TOKEN,
        outcome: Outcome::Ready,
        at: Instant::from_nanos_since_boot(at),
    }
}

/// **The publication.** Whatever the schedule, a record the taker gets back is
/// the record the poster wrote — never a half-written slot and never the
/// zeroed one the ring started in.
///
/// This is the model the `inbox-release-off` control reds: with the `tail`
/// store relaxed, the slot write and the read below are unordered and loom
/// says so.
#[test]
fn a_record_reaches_its_taker_intact() {
    loom::model(|| {
        let inbox = Arc::new(Inbox::new());

        let poster = inbox.clone();
        let p = loom::thread::spawn(move || poster.post(record(7)));

        let taker = inbox.clone();
        let t = loom::thread::spawn(move || taker.has_record().then(|| taker.take()).flatten());

        let taken = t.join().unwrap();
        p.join().unwrap();

        if let Some(got) = taken {
            assert!(
                got.token == TOKEN && got.at.nanos_since_boot() == 7,
                "a taker read a slot its poster had not finished writing",
            );
        }
    });
}

/// **A post that ran entirely before the arm is still found.** The record is
/// level-readable — it stays until its owner takes it — which is what lets the
/// park-time recheck be one predicate with nothing named in it.
#[test]
fn a_post_before_the_arm_is_still_there_after_it() {
    loom::model(|| {
        let inbox = Inbox::new();
        inbox.post(record(1));
        assert!(inbox.has_record(), "a record was lost between the post and the arm");
        let got = inbox.take().expect("has_record said there was one");
        assert!(got.at.nanos_since_boot() == 1);
        assert!(!inbox.has_record(), "the ring still claims a record after the only one was taken");
    });
}

/// **Invariant W.** A poster stores the record and *then* claims the waiter; a
/// parker registers and publishes that it is about to park, and *then*
/// rechecks. The run may not end with the poster having claimed nobody **and**
/// the parker having parked.
///
/// **The subject's leaf lock is in the model because it is in the proof.** §5.4
/// puts C1 — the record store — *under* that lock, and the poster's walk of the
/// watch list is under it too; a parker registers under the same lock before it
/// rechecks. That is what closes the window this file's first two drafts
/// reported: a poster that finds no waiter registered has already stored its
/// record, so the parker's registration happens after that store and its
/// recheck must see it. Model the claim as a bare word CAS with no list behind
/// it and loom finds a lost wake the real protocol cannot produce.
///
/// The word has three states for the same reason it has three in `toyos-sched`:
/// a poster arriving after the commit finds `Blocked`, and its wake becomes a
/// message to the owning CPU rather than a claim with nothing to claim.
#[test]
fn a_post_racing_a_park_leaves_nobody_waiting() {
    const IDLE: usize = 0;
    const COMMITTING: usize = 1;
    const BLOCKED: usize = 2;

    loom::model(|| {
        struct Machine {
            inbox: Inbox,
            /// The subject's leaf lock, and the watch list under it: `true` is
            /// "this task's node is on the list".
            list: Mutex<bool>,
            /// Stands in for `toyos_sched`'s rendezvous word and nothing more.
            word: AtomicUsize,
            claimed: AtomicBool,
        }

        let m = Arc::new(Machine {
            inbox: Inbox::new(),
            list: Mutex::new(false),
            word: AtomicUsize::new(IDLE),
            claimed: AtomicBool::new(false),
        });

        let poster = m.clone();
        let p = loom::thread::spawn(move || {
            let armed = {
                let list = poster.list.lock().unwrap();
                // C1: the record, under the subject's leaf lock.
                poster.inbox.post(record(2));
                *list
            };
            if armed {
                // C2: claim the waiter, in whichever state the word is in.
                for from in [COMMITTING, BLOCKED] {
                    if poster
                        .word
                        .compare_exchange(from, IDLE, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                    {
                        poster.claimed.store(true, Ordering::SeqCst);
                        break;
                    }
                }
            }
        });

        let parker = m.clone();
        let k = loom::thread::spawn(move || {
            {
                // P1: register, and publish that this task is about to park.
                let mut list = parker.list.lock().unwrap();
                *list = true;
                parker.word.store(COMMITTING, Ordering::SeqCst);
            }
            // P2: the one recheck, and the whole of it.
            if parker.inbox.has_record() {
                parker.word.store(IDLE, Ordering::SeqCst);
                false
            } else {
                // The commit itself: a poster that got here first has already
                // taken the word, and the park is refused.
                parker
                    .word
                    .compare_exchange(COMMITTING, BLOCKED, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            }
        });

        let parked = k.join().unwrap();
        p.join().unwrap();

        assert!(
            !parked || m.claimed.load(Ordering::SeqCst),
            "the poster claimed nobody and the parker parked: a record is left under a \
             parked task, which is a wait for something that has already happened",
        );
    });
}

/// **Two posters, one taker, under the lock the subject already holds.** The
/// inbox's `tail` is a plain store precisely because the walk that reaches it
/// is serialized; this is the model of that claim. Both records arrive, in
/// some order, and neither is torn.
#[test]
fn two_posters_under_the_subject_lock_lose_nothing() {
    loom::model(|| {
        let inbox = Arc::new(Mutex::new(Inbox::new()));

        let a = inbox.clone();
        let ta = loom::thread::spawn(move || a.lock().unwrap().post(record(1)));
        let b = inbox.clone();
        let tb = loom::thread::spawn(move || b.lock().unwrap().post(record(2)));

        ta.join().unwrap();
        tb.join().unwrap();

        let guard = inbox.lock().unwrap();
        let mut seen = 0u64;
        while let Some(got) = guard.take() {
            assert!(got.token == TOKEN);
            seen += got.at.nanos_since_boot();
        }
        assert_eq!(seen, 3, "both posts must be readable, and each exactly once");
    });
}

/// **A full inbox loses records and never a wake.** Two slots under loom, three
/// posts: the taker gets what fits and then one `Gone(Overflowed)`, which is
/// what tells it to re-derive its own predicate.
#[test]
fn an_overflow_is_reported_once_and_then_cleared() {
    loom::model(|| {
        let inbox = Inbox::new();
        for at in 1..=3 {
            inbox.post(record(at));
        }
        let mut records = 0;
        let mut overflows = 0;
        while let Some(got) = inbox.take() {
            match got.outcome {
                Outcome::Ready => records += 1,
                Outcome::Gone(_) => overflows += 1,
            }
        }
        assert_eq!((records, overflows), (2, 1), "a full ring drops records, with a notice");
        assert!(!inbox.has_record(), "the overflow notice was reported twice");
    });
}
