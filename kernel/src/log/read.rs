//! The reader a dying machine uses.
//!
//! One function, and it is the one the panic console and Ctrl+Alt+D call: every
//! *committed* record in a window, merged across shards, taking no lock and
//! never blocking. A slot a writer is inside is skipped rather than waited for,
//! because the writer that would have finished it may be the CPU that just
//! halted — and a reader that waits for it never reaches the line that says
//! why.
//!
//! **The streaming reader is not here.** `drain_ordered` blocks a shard at its
//! first uncommitted record, which is right for `klogd` and for the cursor
//! syscall and wrong for a machine that has no "later"; it arrives with the
//! first of those callers rather than as a function nothing calls.
//!
//! `specs/log-architecture-spec.md` §3.1.

use toyos_abi::log::{LogRecord, MAX_LOG_SHARDS};

use super::shard::Shard;

/// Somewhere a whole record goes.
///
/// `false` ends the walk *and means the record was not taken*. The caller is a
/// fixed panel buffer and the walk is newest-first, so "no more room" is the
/// natural end of it — there is no separate truncation for anybody to detect
/// afterwards.
pub trait RecordSink {
    fn put(&mut self, record: &LogRecord) -> bool;
}

/// One shard's descent, and the candidate it is offering the merge.
///
/// It carries a sequence number and a timestamp rather than a record, which is
/// what keeps eight candidates inside 256 bytes: a [`LogRecord`] is a kilobyte
/// and the double-fault stack is 16 KiB with a crash report already on it.
#[derive(Clone, Copy)]
struct Descent {
    shard: Option<&'static Shard>,
    /// The next sequence number to try, descending.
    next: u64,
    /// Where the descent stops: the oldest number this shard can answer for.
    floor: u64,
    /// This shard's candidate — its sequence number and the key it is compared
    /// on — or `None` when the shard has nothing left in the window.
    cand: Option<(u64, u64)>,
}

const IDLE: Descent = Descent { shard: None, next: 0, floor: u64::MAX, cand: None };

/// Eight of these are the whole of the merge's state, and the number is what
/// says the panic path can afford it. **Measured, not estimated** — a `const`
/// assertion rather than a comment, so it cannot drift:
/// `size_of::<Descent>()` is 48 and eight is 384 bytes, against IST1's 16 KiB.
const _: () = assert!(core::mem::size_of::<Descent>() == 48);
const _: () = assert!(core::mem::size_of::<[Descent; MAX_LOG_SHARDS]>() == 384);

impl Descent {
    /// Take the next candidate at or below [`Descent::next`], or leave the
    /// shard with none.
    ///
    /// **`from` stops the descent, and that rests on where `emit` stamps.** A
    /// record's `at_ns` is read inside the same IF/TF-off bracket as its
    /// reservation, one instruction apart, so within a shard the sequence order
    /// *is* the timestamp order and everything below a record older than `from`
    /// is older still. Stamped before the bracket — which is where it was until
    /// this reader existed — an interrupted producer could give the lower
    /// sequence number the later timestamp, and this early stop would then drop
    /// live records: a CPU that was mid-`emit` when Ctrl+Alt+D took its `from`
    /// would lose its whole answer. `log/mod.rs`'s `emit` carries the other half
    /// of this argument.
    ///
    /// `to` only skips, because above the window there is no such argument to
    /// make: a caller asking for a bracket that closed a moment ago is walking
    /// down through records that arrived since.
    fn advance(&mut self, from: u64, to: u64) {
        self.cand = None;
        let Some(shard) = self.shard else { return };
        while self.next >= self.floor {
            let seq = self.next;
            self.next = seq - 1;
            let Some(at_ns) = shard.at_ns(seq) else { continue };
            if at_ns > to {
                continue;
            }
            if at_ns < from {
                return;
            }
            self.cand = Some((seq, at_ns));
            return;
        }
    }
}

/// Every committed record stamped in `from..=to`, **newest first**, merged
/// across shards by `at_ns`.
///
/// Newest first because every caller has a fixed buffer and shows the *end* of
/// what it holds: a panel that filled from the oldest end would spend its
/// buffer on the boot and drop the panic. It also bounds the work by the
/// buffer instead of by the ring — the sink says "full" and the walk stops,
/// rather than every call copying every live record out of every shard.
///
/// **It returns nothing.** A count of records emitted is a number no caller
/// has: the panel measures what it rendered in bytes, and the streaming reader
/// that will want a count is L3's. Returning one now would be a contract
/// nothing checks.
///
/// Takes no lock and allocates nothing: one [`Descent`] per shard on the stack,
/// pick the newest, copy that one record, repeat.
pub fn snapshot_committed(from: u64, to: u64, out: &mut impl RecordSink) {
    let mut descents = [IDLE; MAX_LOG_SHARDS];
    for (descent, shard) in descents.iter_mut().zip(super::shards()) {
        let Some(shard) = shard else { continue };
        // `head` counts reservations, so the newest number that can have a
        // record is one below it.
        descent.shard = Some(shard);
        descent.next = shard.head().saturating_sub(1);
        descent.floor = shard.oldest_readable();
        descent.advance(from, to);
    }

    loop {
        let mut best: Option<(usize, u64)> = None;
        for (i, descent) in descents.iter().enumerate() {
            if let Some((_, at_ns)) = descent.cand {
                if best.is_none_or(|(_, newest)| at_ns > newest) {
                    best = Some((i, at_ns));
                }
            }
        }
        let Some((i, _)) = best else { return };
        let Some(descent) = descents.get_mut(i) else { return };
        let Some((seq, _)) = descent.cand else { return };

        // The one place a record is copied, and only after the comparison has
        // chosen it. `None` here is a writer that recycled the slot between the
        // key and the body — it is newer than everything left, so nothing is
        // emitted out of order by skipping it; the record is simply gone.
        let copied = descent.shard.and_then(|shard| shard.read(seq));
        descent.advance(from, to);
        if let Some(record) = copied {
            if !out.put(&record) {
                return;
            }
        }
    }
}
