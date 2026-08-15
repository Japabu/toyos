//! The two readers, because a dying machine and a live one want different
//! rules.
//!
//! [`snapshot_committed`] is the panic console's and Ctrl+Alt+D's: every
//! *committed* record in a window, newest first, taking no lock and never
//! blocking. A slot a writer is inside is skipped rather than waited for,
//! because the writer that would have finished it may be the CPU that just
//! halted — and a reader that waits for it never reaches the line that says
//! why.
//!
//! [`drain_ordered`] is `klogd`'s and, from L4, `SYS_LOG_READ`'s: in sequence
//! order per shard, oldest first, stopping a shard at its first uncommitted
//! record. On a live machine that stop is one bounded 1 KiB publication long;
//! on a dying one it can be forever, which is exactly why the machine that is
//! halting uses the other function instead.
//!
//! **Two names rather than one with a flag**, because each is correct for its
//! caller and neither is a mode of the other.
//!
//! `specs/log-architecture-spec.md` §3.1.

use toyos_abi::log::{LogRecord, MAX_LOG_SHARDS};

use super::shard::{Shard, FIRST_SEQ};

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
/// what keeps eight candidates inside the 384 bytes the assertion below pins:
/// a [`LogRecord`] is a kilobyte, so eight of those would be 8 KiB of a
/// double-fault stack that has 16 KiB and a crash report already on it.
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

/// Where one reader has got to, and how much it never saw.
///
/// **The kernel holds none of these.** `klogd` owns one on its own stack and
/// `SYS_LOG_READ`'s caller owns the other in its own memory, so there is no
/// object, no handle lifecycle, nothing to leak or go stale, and a second
/// reader costs nothing.
pub struct Cursor {
    /// The next sequence number wanted from each shard.
    next: [u64; MAX_LOG_SHARDS],
    /// Records this cursor never saw because they were overwritten.
    ///
    /// **Derived, never counted.** There is no producer-side drop counter
    /// anywhere in this design: loss is `oldest_readable - next`, computed from
    /// two numbers that already have to be right, so nothing can drift from the
    /// ring. It also keeps the last `fetch_add` off the overflow path.
    lost: u64,
}

impl Cursor {
    /// A reader that has seen nothing. [`FIRST_SEQ`] and not zero: zero is the
    /// state of a slot nothing has written, so a cursor starting there would
    /// count one phantom loss per shard on its first call.
    pub const fn new() -> Self {
        Self { next: [FIRST_SEQ; MAX_LOG_SHARDS], lost: 0 }
    }

    pub fn lost(&self) -> u64 {
        self.lost
    }

    /// Clamp this shard's position to what it can still answer for, counting
    /// the difference as loss, and offer the candidate now sitting there.
    ///
    /// `None` is "nothing to take from this shard right now" and covers both
    /// reasons at once — the shard is empty, or a writer is inside the slot —
    /// because the caller does the same thing in either case.
    fn open(&mut self, i: usize, shard: Option<&'static Shard>) -> Option<u64> {
        let shard = shard?;
        let oldest = shard.oldest_readable();
        // `max(FIRST_SEQ)` rather than trusting the field: a cursor that
        // crossed the syscall boundary arrives zeroed on its first call, and a
        // zero here would read as "the reader has missed everything".
        let want = self.next.get(i).copied().unwrap_or(FIRST_SEQ).max(FIRST_SEQ);
        self.lost += oldest.saturating_sub(want);
        let want = want.max(oldest);
        *self.next.get_mut(i)? = want;
        shard.at_ns(want)
    }
}

impl Default for Cursor {
    fn default() -> Self {
        Self::new()
    }
}

/// A cursor kept where more than one context can pick it up.
///
/// **The console's drain is the one cursor in the machine that is shared**, and
/// it has to be: `Drain::Inline` advances it from the producer's own stack,
/// `klogd` advances it from a kernel thread, and the panic path advances it
/// from a machine that is stopping. Three cursors over one stream would put
/// every record on the wire three times.
///
/// **Words rather than a `Cursor` behind a cell**, because the panic path's
/// bypass reads the position with no backend lock held — that is what the
/// bypass *is* — and an array of `AtomicU64` has no torn read to reason about.
/// `log_file` keeps its own position the same way for a plainer reason: its
/// pending predicate is read from the pre-`hlt` check with interrupts off and
/// may take no lock.
pub struct Published {
    next: [core::sync::atomic::AtomicU64; MAX_LOG_SHARDS],
    lost: core::sync::atomic::AtomicU64,
}

impl Published {
    pub const fn new() -> Self {
        use core::sync::atomic::AtomicU64;
        Self {
            next: [const { AtomicU64::new(FIRST_SEQ) }; MAX_LOG_SHARDS],
            lost: AtomicU64::new(0),
        }
    }

    /// The position, as a cursor to walk with.
    pub fn take(&self) -> Cursor {
        use core::sync::atomic::Ordering;
        Cursor {
            next: core::array::from_fn(|i| self.next[i].load(Ordering::Relaxed)),
            lost: self.lost.load(Ordering::Relaxed),
        }
    }

    /// Where the walk got to. Whatever exclusion the position has is the
    /// caller's — the backend lock for the console, `SINK`'s for the file —
    /// and these stores only make the result visible to a reader that has none.
    pub fn put(&self, cursor: &Cursor) {
        use core::sync::atomic::Ordering;
        for (word, next) in self.next.iter().zip(cursor.next) {
            word.store(next, Ordering::Relaxed);
        }
        self.lost.store(cursor.lost, Ordering::Relaxed);
    }

    pub fn lost(&self) -> u64 {
        self.lost.load(core::sync::atomic::Ordering::Relaxed)
    }

    /// Forget everything this position has been told, so the next walk starts
    /// at the oldest record every shard still holds.
    ///
    /// **The loss count goes back to zero with it, and that is the honest
    /// reading rather than a convenience.** After a rewind the position means
    /// "nothing has been said to *this* consumer", so what `lost` counts is
    /// what the shards had already dropped before the replay could reach it —
    /// which is a different, and correct, number for the consumer that is about
    /// to hear the boot for the first time. Keeping the old total would count
    /// the same overwritten record twice.
    pub fn rewind(&self) {
        use core::sync::atomic::Ordering;
        for word in &self.next {
            word.store(FIRST_SEQ, Ordering::Relaxed);
        }
        self.lost.store(0, Ordering::Relaxed);
    }

    /// Is there a committed record this position has not taken? Lock-free, for
    /// the callers that ask with interrupts off.
    pub fn any_pending(&self) -> bool {
        any_committed(&self.take())
    }
}

/// Every record this cursor has not seen, **oldest first**, merged across
/// shards by `at_ns`. Returns how many reached `out`.
///
/// **It blocks a shard, never the stream.** A shard stopped at an uncommitted
/// record does not stop the merge: the others keep flowing, and that shard's
/// records arrive on a later call once its writer commits. So the order is
/// `at_ns` order *among what it emits*, which is the one ordering property this
/// design does not give — and the alternative, stalling every shard on the
/// slowest, is what turns one wedged CPU into a silent machine.
///
/// Allocation-free and bounded by [`MAX_LOG_SHARDS`], like its sibling.
pub fn drain_ordered(cursor: &mut Cursor, out: &mut impl RecordSink) -> usize {
    let shards = super::shards();
    let mut cand = [None; MAX_LOG_SHARDS];
    for (i, slot) in cand.iter_mut().enumerate() {
        *slot = cursor.open(i, shards[i]);
    }

    let mut emitted = 0;
    loop {
        // Oldest first, which is the opposite of the snapshot's order and for
        // the opposite reason: this reader has no fixed buffer to spend and its
        // consumer wants the stream in the order it happened.
        let mut best: Option<(usize, u64)> = None;
        for (i, slot) in cand.iter().enumerate() {
            if let Some(at_ns) = *slot {
                if best.is_none_or(|(_, oldest)| at_ns < oldest) {
                    best = Some((i, at_ns));
                }
            }
        }
        let Some((i, _)) = best else { return emitted };
        let Some(shard) = shards[i] else { return emitted };

        match shard.read(cursor.next[i]) {
            Some(record) => {
                if !out.put(&record) {
                    return emitted;
                }
                emitted += 1;
                cursor.next[i] += 1;
            }
            // The slot was recycled between the timestamp and the body. The
            // record is gone; `open` below re-clamps and counts it, which is
            // the same subtraction every other loss goes through.
            //
            // **This arm does not advance `next[i]` and does not have to, and
            // the reason is worth writing down because the shape invites the
            // opposite conclusion.** `open` offered this candidate, so
            // `at_ns(next[i])` answered `Some` — which means `next[i] < head`
            // and the slot held `next[i]`. For `read` to answer `None` a moment
            // later, one of the two tests they share must have flipped, and
            // only one of them can: `head` never shrinks, so `seq >= head`
            // cannot become true; what changed is that a writer reserved
            // `next[i] + SHARD_RECORDS` and entered the slot. That reservation
            // puts `head` at `next[i] + SHARD_RECORDS + 1` or beyond, so
            // `oldest_readable` — which is `head - SHARD_RECORDS` and is also
            // monotonic — is now **strictly greater than `next[i]`**. The
            // `open` call below therefore clamps the position *up*, counting
            // the difference as loss, and the iteration that took this arm
            // still ends with `next[i]` larger than it started. Every arm of
            // this loop either emits, advances, or returns, so a shard cannot
            // stall the drain by losing a race it is losing because it is being
            // written to.
            None => {}
        }
        cand[i] = cursor.open(i, shards[i]);
    }
}

/// Is there a committed record this cursor has not taken?
///
/// The predicate [`drain_ordered`] stops on, asked without taking anything —
/// which is what `shard::arm_waiter`'s rescan needs and why the rescan is over
/// *committed records* rather than over `head`.
pub fn any_committed(cursor: &Cursor) -> bool {
    super::shards()
        .iter()
        .enumerate()
        .any(|(i, shard)| match shard {
            Some(shard) => {
                let want = cursor.next[i].max(FIRST_SEQ).max(shard.oldest_readable());
                shard.at_ns(want).is_some()
            }
            None => false,
        })
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
