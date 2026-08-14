//! Loom: the record ring's publication and recycle edges.
//!
//! **x86 gives every load acquire and every store release semantics, so none of
//! this can fail on the machine this tree is developed on.** A dropped
//! `Release` on the commit store, or a hoisted re-check in the reader, is
//! invisible to every guest test and to CI's KVM shards alike. ARM64 is planned
//! and would not forgive either. This is the only instrument in the tree that
//! sees them.
//!
//! `SHARD_RECORDS` is 4 here, which is what makes the recycle cases reachable
//! in a model at all — `shard.rs` says why.
//!
//! `specs/log-architecture-spec.md` §2.5, obligations W1, W2 and W4.

#![cfg(feature = "loom")]

use kernel_loom::log_shard::{Shard, FIRST_SEQ, SHARD_RECORDS};
use loom::sync::Arc;
use toyos_abi::log::{LogRecord, MAX_RECORD_MESSAGE};

/// The model's witness for the production IF/TF-off bracket.
fn commit_one(shard: &Shard) -> u64 {
    let guard = kernel_loom::arch::LogCommitGuard::close();
    // SAFETY: every caller is this model's sole producer for the shard. The
    // real witness additionally prevents that producer being preempted between
    // these two operations.
    let seq = unsafe { shard.reserve(&guard) };
    unsafe { shard.commit(seq, &record(seq), &guard) };
    seq
}

/// A record whose body is entirely derivable from its sequence number, so a
/// reader that sees a body from a different record fails an equality rather
/// than a plausibility check.
fn record(seq: u64) -> LogRecord {
    let mut r = LogRecord::EMPTY;
    r.seq = seq;
    r.at_ns = seq * 1000 + 7;
    r.pid = seq as u32 + 100;
    r.tid = seq as u32 + 200;
    r.cpu = 3;
    r.len = 8;
    r.msg[..8].copy_from_slice(&seq.to_le_bytes());
    r
}

/// Everything about a record that a torn read could disagree on.
///
/// **Checked as a whole rather than field by field.** The failure this is
/// looking for is a body from one writer mixed with a body from another, and a
/// check of one field cannot see that.
fn assert_whole(got: &LogRecord, seq: u64) {
    let want = record(seq);
    assert_eq!(got.seq, seq, "a slot answered for a sequence number it does not hold");
    assert_eq!(got.at_ns, want.at_ns, "torn: at_ns is another record's");
    assert_eq!(got.pid, want.pid, "torn: pid is another record's");
    assert_eq!(got.tid, want.tid, "torn: tid is another record's");
    assert_eq!(got.cpu, want.cpu, "torn: cpu is another record's");
    assert_eq!(got.len, want.len, "torn: len is another record's");
    assert_eq!(
        got.msg[..MAX_RECORD_MESSAGE.min(8)],
        want.msg[..MAX_RECORD_MESSAGE.min(8)],
        "torn: the message body is another record's"
    );
}

/// **W1 — publication.** The body stores happen-before the commit store, so a
/// reader whose `seq.load(Acquire)` answers `s` sees the body written for `s`.
///
/// A dropped `Release` on the commit lets the reader observe the sequence
/// number with a stale or half-written body behind it. On x86 it cannot; here
/// it can.
#[test]
fn a_committed_record_is_whole_or_absent() {
    loom::model(|| {
        let shard = Arc::new(Shard::new());
        let writer = shard.clone();

        let w = loom::thread::spawn(move || {
            // SAFETY: this thread is the shard's sole writer, which is the
            // precondition the shim's own doc states.
            let guard = kernel_loom::arch::LogCommitGuard::close();
            let seq = unsafe { writer.reserve(&guard) };
            let r = record(seq);
            unsafe { writer.commit(seq, &r, &guard) };
        });

        // The reader races the writer and must see nothing or everything.
        if let Some(got) = shard.read(FIRST_SEQ) {
            assert_whole(&got, FIRST_SEQ);
        }

        w.join().unwrap();
        let got = shard.read(FIRST_SEQ).expect("committed once the writer has joined");
        assert_whole(&got, FIRST_SEQ);
    });
}

/// **W2 — the window, checked as a window.** Everything inside it reads back
/// whole; everything the ring has passed reads back as nothing.
///
/// **Single-threaded, and that is a decision rather than a shortcut.** Loom
/// explores schedules, and a model with two threads and five records each is
/// hundreds of thousands of interleavings — measured here, it did not finish in
/// ten minutes. What this property is about is a *state*: which sequence
/// numbers a shard can answer for once `head` has moved past them. The
/// ordering half of the same mechanism is what
/// [`a_committed_record_is_whole_or_absent`] models, with one record and two
/// threads, which loom finishes in milliseconds.
#[test]
fn the_readable_window_is_exactly_the_last_shard_records() {
    loom::model(|| {
        let shard = Shard::new();
        let total = SHARD_RECORDS as u64 + 1;
        for _ in 0..total {
            commit_one(&shard);
        }

        let head = shard.head();
        assert_eq!(head, FIRST_SEQ + total);
        assert!(shard.read(head).is_none(), "a number that was never issued answered");
        for seq in FIRST_SEQ..head {
            match shard.read(seq) {
                Some(got) => {
                    assert!(seq >= shard.oldest_readable(), "{seq} is past the window");
                    assert_whole(&got, seq);
                }
                None => assert!(seq < shard.oldest_readable(), "{seq} is in the window"),
            }
        }
    });
}

/// **The defect the two-store publish exists to prevent, as a state rather than
/// a schedule.**
///
/// Until 2026-08-11 `commit` wrote the body and *then* stored the sequence
/// number, so throughout the body write the word still read the previous
/// generation's number — and a reader that loaded it, copied a
/// half-overwritten body and re-checked saw the same value twice and accepted
/// the tear. Marking the slot before touching the body is what closes it, and
/// this asserts the mark is observable: mid-recycle, the slot answers for
/// neither generation.
///
/// Deterministic on purpose. The window is a state a writer is in, not a
/// schedule two threads have to be caught in, so a race is the wrong instrument
/// for it — and the reachable cause in the kernel is a `#DB` storm on one CPU,
/// which is not a race either.
#[test]
fn a_recycled_slot_does_not_answer_for_the_record_it_replaced() {
    loom::model(|| {
        let shard = Shard::new();
        for _ in 0..SHARD_RECORDS {
            commit_one(&shard);
        }
        assert_whole(&shard.read(FIRST_SEQ).expect("the first record is still in the window"), FIRST_SEQ);

        // The next reservation lands on the same slot. It is out of the window
        // from the moment `head` moves, before a byte of its body is written —
        // which is the guarantee the reader's lower bound rests on.
        // SAFETY: sole writer.
        let guard = kernel_loom::arch::LogCommitGuard::close();
        let recycling = unsafe { shard.reserve(&guard) };
        assert_eq!(recycling % SHARD_RECORDS as u64, FIRST_SEQ % SHARD_RECORDS as u64);
        assert!(
            shard.read(FIRST_SEQ).is_none(),
            "the slot answered for the record it is about to overwrite"
        );

        // SAFETY: the number came from this shard and is used once.
        unsafe { shard.commit(recycling, &record(recycling), &guard) };
        assert_whole(&shard.read(recycling).expect("the new record is readable"), recycling);
        assert!(shard.read(FIRST_SEQ).is_none(), "the replaced record came back");
    });
}

/// **The half of the validity test that a sentinel would have hidden.**
///
/// Every slot starts zeroed, so slot 0 of a shard nothing has written holds
/// `seq == 0` — which *equals* sequence number 0. Equality alone reads an
/// all-zero record as record 0 of every shard on every boot. `seq < head` is
/// what refuses it, and this is the model that fails if the comparison is ever
/// dropped as redundant.
#[test]
fn a_shard_nothing_has_written_answers_for_nothing() {
    loom::model(|| {
        let shard = Shard::new();
        assert_eq!(shard.head(), FIRST_SEQ);
        assert!(shard.read(0).is_none(), "a zeroed slot answered as record 0");
        assert!(shard.read(FIRST_SEQ).is_none(), "an unissued number answered");
    });
}

/// **W4's precondition, made visible.** `head` is written by one CPU and read
/// by others, and the reader's view of it only ever grows — so
/// `oldest_readable` is a bound a reader can act on rather than a race.
#[test]
fn a_concurrent_reader_sees_head_grow_and_never_shrink() {
    loom::model(|| {
        let shard = Arc::new(Shard::new());
        let writer = shard.clone();

        let w = loom::thread::spawn(move || {
            // SAFETY: sole writer.
            commit_one(&writer);
        });

        let first = shard.head();
        let second = shard.head();
        assert!(second >= first, "head went backwards: {first} then {second}");
        assert!(shard.oldest_readable() <= shard.head());

        w.join().unwrap();
        assert_eq!(shard.head(), FIRST_SEQ + 1);
    });
}

/// **W4b — a key and the record it names are one generation.**
///
/// `Shard::at_ns` is a second reader over the same slot, and it exists because
/// the merge compares timestamps: holding whole records instead would put eight
/// kilobytes on a stack the double-fault path has sixteen of. A second reader is
/// a second chance to get the seqlock wrong, and this is the model that says so
/// — every other model here drives `read`, so all five of them stay green with
/// `at_ns`'s `Acquire` weakened to `Relaxed`.
///
/// The slot is filled with an older generation first, so "stale" and "fresh"
/// are distinguishable rather than merely plausible: a reader that answers with
/// generation one's timestamp for generation two's sequence number would order
/// the merge by a key the record it then copies does not have.
#[test]
fn a_key_and_the_record_it_names_come_from_one_generation() {
    loom::model(|| {
        let shard = Arc::new(Shard::new());
        // One full lap, so the slot `target` lands in already holds a record
        // with a different timestamp in it.
        for _ in 0..SHARD_RECORDS {
            commit_one(&shard);
        }
        let target = FIRST_SEQ + SHARD_RECORDS as u64;

        let writer = shard.clone();
        let w = loom::thread::spawn(move || {
            // SAFETY: this thread is the shard's sole writer.
            let guard = kernel_loom::arch::LogCommitGuard::close();
            let seq = unsafe { writer.reserve(&guard) };
            let r = record(seq);
            unsafe { writer.commit(seq, &r, &guard) };
        });

        // Racing the recycle: nothing, or this generation's own key.
        if let Some(at_ns) = shard.at_ns(target) {
            assert_eq!(
                at_ns,
                record(target).at_ns,
                "a key from the generation the slot used to hold"
            );
        }

        w.join().unwrap();
        assert_eq!(shard.at_ns(target), Some(record(target).at_ns));
        // And the key the merge orders by is the one the copy it then makes
        // carries, which is the property the two readers exist to share.
        let got = shard.read(target).expect("committed once the writer has joined");
        assert_eq!(shard.at_ns(target), Some(got.at_ns));
    });
}
