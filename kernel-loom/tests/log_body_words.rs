//! Host-fast regression for the packing a slot's body is.
//!
//! A slot holds atomic machine words, not a struct: three identity words and
//! then the message, little-endian. That packing is written out by hand in
//! `shard.rs` — deliberately, so the agreement between the writer and the reader
//! is a property of that file rather than of the compiler's layout choice — and
//! **hand-written packing is exactly the kind of code a round trip has to
//! check**. A field shifted into the wrong half of a word is invisible to every
//! model here: loom explores orderings, and a consistently mis-packed record is
//! perfectly ordered.
//!
//! `--no-default-features`, for [`log_zeroed_init`]'s reason and one more of its
//! own: `MSG_WORDS` is 1 under loom, so the loom build cannot carry a message
//! wider than eight bytes at all and the full-width case is unreachable there.
//!
//! [`log_zeroed_init`]: ../log_zeroed_init.rs

#![cfg(not(feature = "loom"))]

use kernel_loom::log_shard::Shard;
use toyos_abi::log::{LogRecord, Level, FLAG_EARLY, MAX_RECORD_MESSAGE};

/// A record whose every field is distinct, so a swap between two of them fails
/// rather than being absorbed.
fn record(seq: u64, len: usize) -> LogRecord {
    let mut r = LogRecord::EMPTY;
    r.seq = seq;
    r.at_ns = 0x0123_4567_89ab_cdef;
    r.pid = 0xdead_beef;
    r.tid = 0xfeed_face;
    r.cpu = 0x1234;
    r.len = len as u16;
    r.elided = 0x5678;
    r.level = Level::Alert as u8;
    r.flags = FLAG_EARLY;
    for (i, b) in r.msg[..len].iter_mut().enumerate() {
        *b = b'a' + (i % 26) as u8;
    }
    r
}

fn round_trip(len: usize) {
    let shard = Shard::new();
    let guard = kernel_loom::arch::LogCommitGuard::close();
    // SAFETY: this host thread is the shard's only producer, and each sequence
    // number is committed once.
    let seq = unsafe { shard.reserve(&guard) };
    let want = record(seq, len);
    unsafe { shard.commit(seq, &want, &guard) };
    drop(guard);

    let got = shard.read(seq).expect("the record just committed must be readable");
    assert_eq!(got.seq, want.seq, "seq");
    assert_eq!(got.at_ns, want.at_ns, "at_ns");
    assert_eq!(got.pid, want.pid, "pid");
    assert_eq!(got.tid, want.tid, "tid");
    assert_eq!(got.cpu, want.cpu, "cpu");
    assert_eq!(got.len, want.len, "len");
    assert_eq!(got.elided, want.elided, "elided");
    assert_eq!(got.level, want.level, "level");
    assert_eq!(got.flags, want.flags, "flags");
    assert_eq!(got.message(), want.message(), "the message");
    // And the key the merge orders by is the one the copy carries, read through
    // the other reader over the same words.
    assert_eq!(shard.at_ns(seq), Some(want.at_ns), "at_ns through the key reader");
}

/// Every field, at the three lengths the word arithmetic can disagree about:
/// nothing, a partial tail word, and the bound itself.
#[test]
fn every_field_survives_the_word_packing() {
    round_trip(0);
    round_trip(1);
    round_trip(7);
    round_trip(8);
    round_trip(9);
    round_trip(MAX_RECORD_MESSAGE - 1);
    round_trip(MAX_RECORD_MESSAGE);
}

/// **The writer stores the words the message occupies and no more**, so the
/// bytes past `len` are whatever the slot last held — and a reader must not
/// present them. `LogRecord::message` is bounded by `len`, and this is the test
/// that says the shortening of a slot's contents cannot leak the older, longer
/// record's tail into the new one's message.
#[test]
fn a_shorter_record_does_not_inherit_the_longer_one_it_replaced() {
    let shard = Shard::new();
    let guard = kernel_loom::arch::LogCommitGuard::close();
    // SAFETY: sole producer, each number committed once.
    let first = unsafe { shard.reserve(&guard) };
    unsafe { shard.commit(first, &record(first, 64), &guard) };
    let second = unsafe { shard.reserve(&guard) };
    let mut short = record(second, 3);
    short.msg[..3].copy_from_slice(b"xyz");
    unsafe { shard.commit(second, &short, &guard) };
    drop(guard);

    let got = shard.read(second).expect("the second record is readable");
    assert_eq!(got.message(), "xyz");
    assert_eq!(got.len, 3);
}
