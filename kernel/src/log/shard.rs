//! One CPU's ring of whole records, and the two operations that make "half a
//! record" untypeable.
//!
//! **This file is compiled a second time by `kernel-loom`**, so it may name
//! only what that crate shims: the atomics and `arch::percpu_fetch_add`. That
//! is not a style rule — x86's TSO gives every
//! load acquire and every store release semantics, so a missing edge here is
//! invisible to every guest test, and loom is the only instrument in this tree
//! that can see one. **ARM64 is planned**, and on it the missing edge is not
//! hypothetical. If this file grows a dependency on a subject, the model stops
//! compiling and the ordering stops being checked by anything.
//!
//! `specs/log-architecture-spec.md` §2.2, §2.4 and §2.5.

#[cfg(not(feature = "loom"))]
use core::sync::atomic::{fence, AtomicBool, AtomicU64, Ordering};

#[cfg(feature = "loom")]
use loom::sync::atomic::{fence, AtomicBool, AtomicU64, Ordering};

use toyos_abi::log::{LogRecord, MAX_RECORD_MESSAGE};
/// Only the layout assertions name it, and those are the kernel build's.
#[cfg(not(feature = "loom"))]
use toyos_abi::log::RECORD_BYTES;

/// Slots per CPU: 512 KiB at `RECORD_BYTES` of 1024, and 4 MiB at the shipped
/// eight. `specs/log-architecture-spec.md` §13.3 is the ruling that bought it.
///
/// **Sized by records emitted before a reader exists**, which is the only
/// quantity this bound has to cover — after that `klogd` and `/bin/logd` are
/// draining. Measured over all eighteen committed T14 logs, cpu0 and `boot`
/// records up to and including `Boot: complete`: **184 to 186**, and 185 in
/// fifteen of the eighteen. This is that with 2.7x of headroom.
///
/// Every other shard has a reader runnable within a scheduler pass, so no AP
/// shard has to hold a boot. One constant rather than two: giving APs 128 slots
/// saves 0.7 MiB and costs a runtime mask.
#[cfg(not(feature = "loom"))]
pub const SHARD_RECORDS: usize = 512;

/// **Four under loom, and shrinking it is what makes the recycle properties
/// expressible at all.** A model that had to emit 512 records to lap a
/// reservation would explore an unbounded branch and never finish; at four, W2
/// is a handful of steps. Nothing the models check
/// depends on the value — the validity test is exact equality against a
/// `u64` that never wraps, and `seq % SHARD_RECORDS` is the only place the
/// number appears.
#[cfg(feature = "loom")]
pub const SHARD_RECORDS: usize = 4;

/// The record's identity, packed into the three words that precede its message.
///
/// Written out by hand rather than transmuted from a struct: the packing is
/// what the two sides agree on, and a `transmute` would make that agreement a
/// property of the compiler's layout choice instead of of this file.
const HEADER_WORDS: usize = 3;

/// Message words a slot holds.
///
/// **One under loom, for [`SHARD_RECORDS`]'s reason.** A model shard declares
/// `SHARD_RECORDS * (1 + HEADER_WORDS + MSG_WORDS)` loom atomics and builds
/// them all on a 32 KiB generator stack; at the kernel's 124 that is 508 per
/// shard and the model cannot be constructed at all. Nothing the models check
/// depends on the number — every record they write has `len` of 8, which is one
/// word — and the clamp in [`Shard::commit`] is the same expression in both
/// builds, so the model's bound is checked by the same line the kernel's is.
#[cfg(not(feature = "loom"))]
const MSG_WORDS: usize = MAX_RECORD_MESSAGE / 8;
#[cfg(feature = "loom")]
const MSG_WORDS: usize = 1;

/// The whole body: everything in a [`LogRecord`] past the word the writer
/// publishes with.
const BODY_WORDS: usize = HEADER_WORDS + MSG_WORDS;

/// The message bound this file enforces, which is the ABI's in the kernel build
/// and the model's own under loom.
const MSG_BYTES: usize = MSG_WORDS * 8;

#[cfg(not(feature = "loom"))]
const _: () = assert!(MSG_BYTES == MAX_RECORD_MESSAGE);
#[cfg(not(feature = "loom"))]
const _: () = assert!(BODY_WORDS * 8 == RECORD_BYTES - core::mem::size_of::<u64>());

/// The store that publishes a record, and what it carries.
///
/// It is the last store [`Shard::commit`] makes, and the release is what puts
/// every body word ahead of it for a reader whose `slot.seq.load(Acquire)`
/// answers `seq` — obligation W1 of `specs/log-architecture-spec.md` §2.5.
///
/// **A cargo feature rather than a comment, because a model that has never
/// failed proves nothing.** `kernel-loom`'s `log-commit-release-off` makes it
/// `Relaxed` and `kernel-loom/tests/log_record.rs` must red under it: a reader
/// then observes the sequence number with a stale or half-written body behind
/// it, twice over — the re-check reads the same relaxed word and accepts the
/// mixture. On x86 every store is a release and this cannot happen, which is
/// the whole reason W1 is a model. No kernel build can turn the name on: the
/// kernel declares it only so `cfg` checking knows it.
#[cfg(not(feature = "log-commit-release-off"))]
const PUBLISH: Ordering = Ordering::Release;
#[cfg(feature = "log-commit-release-off")]
const PUBLISH: Ordering = Ordering::Relaxed;

/// The three identity words, in the order a slot holds them.
fn header(record: &LogRecord, len: u16) -> [u64; HEADER_WORDS] {
    [
        record.at_ns,
        record.pid as u64 | (record.tid as u64) << 32,
        record.cpu as u64
            | (len as u64) << 16
            | (record.elided as u64) << 32
            | (record.level as u64) << 48
            | (record.flags as u64) << 56,
    ]
}

/// How many message words a record of this length occupies.
///
/// **The writer stores these and the reader loads these, and neither touches
/// the rest.** A record's mean message over the measured corpus is 68 bytes,
/// which is **nine of the 124 message words**; [`HEADER_WORDS`] is always
/// stored, so the publication a producer pays for is **twelve of the body's
/// 127** — the bound is the tail of the distribution and the cost is the record
/// in hand. (This said "nine rather than 127" until 2026-08-15, which compares
/// message words against body words and drops the header from the count.)
fn msg_words(len: u16) -> usize {
    (len as usize).min(MSG_BYTES).div_ceil(8)
}

/// The first sequence number any shard issues.
///
/// **One rather than zero, because every slot starts zeroed.** cpu0's shard is
/// `.bss` and an AP's is `alloc_zeroed`, so slot 0 of a shard nothing has ever
/// written holds the word 0 — which would *equal* sequence number 0 and make a
/// reader accept an all-zero record as record 0 of every shard on every boot.
/// Starting at 1 means no issued number can collide with the zeroed state, and
/// it costs nothing: [`Shard::head`] starts here instead of at zero.
///
/// `kernel-loom`'s `a_shard_nothing_has_written_answers_for_nothing` is the
/// model that fails if this goes back to zero.
pub const FIRST_SEQ: u64 = 1;

/// A slot whose body is being written right now, and therefore holds no record
/// anybody may read.
///
/// **Not a sentinel smuggled in through the back door — it is the second state
/// this one atomic word has to be able to express**, and there is nowhere else
/// to express it: a reader has to learn "a writer is in this slot" from a
/// single atomic load, so a second word would be a second thing that can
/// disagree with the first. `u64::MAX` is unreachable as a sequence number by
/// 2^64 records. It is decoded here, at the boundary, and never carried
/// inward: [`Shard::read`] answers `None` and no [`LogRecord`] ever holds it.
const WRITING: u64 = u64::MAX;

/// One record's storage. **The same layout as [`LogRecord`], as atomic machine
/// words**, and nothing else differs.
///
/// **Every word of it is an `AtomicU64`, and that is a soundness requirement
/// rather than a style.** The body was an `UnsafeCell<Body>` until 2026-08-14:
/// the writer stored the whole struct through it while a reader took a
/// `read_volatile` of the same bytes, and the sequence re-check discarded the
/// torn *result* without ever legalising the *access* — a non-atomic write
/// racing a read is undefined in Rust's model whatever x86 makes of it, and
/// `volatile` is not a synchronisation primitive. Per-word `Relaxed` stores and
/// loads inside the unchanged sequence protocol have no race to discard: on x86
/// each is the same `mov` the struct copy was made of, and the fences that
/// order them are the ones that were already here.
#[repr(C, align(64))]
pub struct Slot {
    /// The state word: a sequence number, or [`WRITING`], or zero for a slot
    /// nothing has touched. The identity and the validity are the same word, so
    /// there is no separate valid flag that could disagree with it.
    seq: AtomicU64,
    /// Three identity words and then the message, little-endian, packed by
    /// [`header`] and unpacked by [`Shard::read`].
    body: [AtomicU64; BODY_WORDS],
}

/// **The layout assertions are the kernel's and are skipped under loom**, whose
/// atomics and cells carry tracking state and are wider than the real ones.
/// Nothing is weakened: the layout binds the build whose layout matters, and the
/// model is about the ordering. The `LogRecord` one holds either way — it is the
/// ABI type, identical in both builds, and it is what says the body starts where
/// the publishing word ends.
#[cfg(not(feature = "loom"))]
const _: () = assert!(core::mem::size_of::<Slot>() == RECORD_BYTES);
#[cfg(not(feature = "loom"))]
const _: () = assert!(core::mem::align_of::<Slot>() == 64);
#[cfg(not(feature = "loom"))]
const _: () = assert!(core::mem::offset_of!(Slot, seq) == 0);
const _: () = assert!(core::mem::offset_of!(LogRecord, at_ns) == core::mem::size_of::<u64>());

/// One CPU's records.
#[repr(C, align(64))]
pub struct Shard {
    /// Reservation counter: the next sequence number this shard will issue.
    /// **Only the owning CPU writes it**; every other CPU reads.
    ///
    /// It counts *reservations*, not commits, so `seq < head` says the number
    /// was handed out and never that the record is there — which is why it is
    /// only half of [`Shard::read`]'s test.
    head: AtomicU64,
    slots: [Slot; SHARD_RECORDS],
}

#[cfg(not(feature = "loom"))]
const _: () = assert!(core::mem::offset_of!(Shard, head) == 0);

impl Shard {
    #[cfg(not(feature = "loom"))]
    pub const fn new() -> Self {
        const EMPTY: Slot = Slot {
            seq: AtomicU64::new(0),
            body: [const { AtomicU64::new(0) }; BODY_WORDS],
        };
        Self { head: AtomicU64::new(FIRST_SEQ), slots: [EMPTY; SHARD_RECORDS] }
    }

    /// Loom's atomics have no `const` constructor, so the model builds shards at
    /// run time.
    #[cfg(feature = "loom")]
    pub fn new() -> Self {
        Self {
            head: AtomicU64::new(FIRST_SEQ),
            slots: core::array::from_fn(|_| Slot {
                seq: AtomicU64::new(0),
                body: core::array::from_fn(|_| AtomicU64::new(0)),
            }),
        }
    }

    /// How many records this shard has ever reserved.
    ///
    /// `Acquire` because a reader pairs it with the commit store: having
    /// established `s < head`, the body it then reads must be the one that
    /// writer wrote.
    pub fn head(&self) -> u64 {
        self.head.load(Ordering::Acquire)
    }

    /// The oldest sequence number this shard can still answer for. Everything
    /// below it has been overwritten, or was never issued.
    pub fn oldest_readable(&self) -> u64 {
        self.head().saturating_sub(SHARD_RECORDS as u64).max(FIRST_SEQ)
    }

    /// Finish constructing a shard obtained from zeroed allocation.
    ///
    /// Zero is the valid empty state of every slot, but it is not the initial
    /// reservation counter: issued sequence numbers start at [`FIRST_SEQ`].
    /// Keeping this as an in-place constructor avoids materialising a 512 KiB
    /// [`Shard`] on the BSP's 16 KiB stack.
    ///
    /// # Safety
    /// `ptr` must point to a zeroed, properly aligned, unpublished allocation
    /// large enough for one [`Shard`]. It may be called exactly once.
    #[cfg(not(feature = "loom"))]
    pub unsafe fn initialize_zeroed(ptr: *mut Self) {
        unsafe {
            core::ptr::addr_of_mut!((*ptr).head).write(AtomicU64::new(FIRST_SEQ));
        }
    }

    /// Take the next sequence number, **on the CPU that owns this shard**.
    ///
    /// One non-`lock`-prefixed `xadd`, which is atomic against an interrupt on
    /// its own CPU — instructions retire whole — and **not** atomic against
    /// another CPU. The [`crate::arch::LogCommitGuard`] passed to
    /// `arch::percpu_fetch_add` is what makes the second half true;
    /// `log/mod.rs`'s `reserve` is the only caller and §2.3a is the argument.
    ///
    /// # Safety
    /// The caller must be the CPU this shard belongs to, and `guard` must stay
    /// live through the matching [`Shard::commit`].
    pub unsafe fn reserve(&self, guard: &crate::arch::LogCommitGuard) -> u64 {
        crate::arch::percpu_fetch_add(&self.head, guard)
    }

    /// Write a record's body into the slot `seq` names and publish it.
    ///
    /// **It takes a whole [`LogRecord`] and never a field at a time**, which is
    /// what makes the caller unable to publish a record it only half built: the
    /// smallest thing this module accepts is one record.
    ///
    /// Two stores bracket the body — [`WRITING`] before it and the sequence
    /// number after — and the comment inside says why one is not enough.
    ///
    /// # Safety
    /// `seq` must have come from this shard's own [`Shard::reserve`], `guard`
    /// must be the same live guard passed to that call, and this must be the
    /// first call for `seq`.
    pub unsafe fn commit(
        &self,
        seq: u64,
        record: &LogRecord,
        _guard: &crate::arch::LogCommitGuard,
    ) {
        debug_assert!(
            self.head().saturating_sub(seq) < SHARD_RECORDS as u64,
            "reservation {seq} was lapped inside its publication bracket: an IF-ignoring path emitted a whole shard generation before this commit"
        );

        let slot = &self.slots[(seq % SHARD_RECORDS as u64) as usize];

        // **Two stores publish, not one, and the first is what makes the
        // reader's re-check total.** Until 2026-08-11 this wrote the body and
        // then stored `seq`, on the argument that "the only thing that can
        // change a slot's body is a writer reserving `seq + SHARD_RECORDS`, and
        // that writer's own commit store changes `slot.seq` away from `seq`".
        // The store comes *after* the body write, so throughout it the word
        // still reads the *previous* generation's number — and a reader that
        // loaded it, copied a half-overwritten body and re-checked saw the same
        // value both times and accepted the tear. The loom recycle models
        // found it on their first run; no guest test can, on any machine.
        // `a_reader_racing_a_recycle_gets_nothing_rather_than_a_mixture`
        // (2026-08-15) is the model that pins this mark directly — it reds if
        // the mark, its release fence, or either reader's acquire fence is
        // removed, and §2.5 records the four weakenings.
        //
        // The release fence is what puts this store ahead of the body writes
        // for the reader, rather than merely ahead of them in this function.
        slot.seq.store(WRITING, Ordering::Relaxed);
        fence(Ordering::Release);

        // The live guard makes this sequence number and slot exclusively ours
        // until we publish it, and the slot now reads `WRITING`, so no reader
        // will accept anything it finds here. Each word is `Relaxed` because the
        // fence above and the release store below are what order the whole body
        // against the sequence number — a per-word ordering would say nothing
        // extra and cost a barrier per word.
        let len = record.len.min(MSG_BYTES as u16);
        for (word, value) in slot.body.iter().zip(header(record, len)) {
            word.store(value, Ordering::Relaxed);
        }
        let words = msg_words(len);
        for i in 0..words {
            // **The nesting gate's injection point, and it is here rather than
            // anywhere tidier because "mid-body" is the whole claim** (§9.2):
            // `log-nested-emit` sends this CPU its own IPI from exactly here,
            // and whether it is delivered before this loop finishes is decided
            // by §2.3a's bracket and by nothing else. `const fn … { false }` in
            // a shipping kernel and in `kernel-loom`'s shim, so this folds to
            // the loop it is written inside.
            if i * 2 == words && crate::actuator::log_nested_emit() {
                super::nested::mid_body();
            }
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&record.msg[i * 8..i * 8 + 8]);
            slot.body[HEADER_WORDS + i].store(u64::from_le_bytes(bytes), Ordering::Relaxed);
        }

        // The store that publishes, and it is the last one.
        slot.seq.store(seq, PUBLISH);
    }

    /// The timestamp of record `seq`, under exactly [`Shard::read`]'s validity
    /// test and without copying the body.
    ///
    /// **It exists so a merge does not cost a kilobyte per candidate.** A
    /// newest-first merge across [`MAX_LOG_SHARDS`] shards holds one candidate
    /// per shard at all times, and a candidate is only ever compared by its
    /// `at_ns` — so holding whole [`LogRecord`]s would put 8 KiB on a stack the
    /// double-fault path has 16 KiB of. The chosen record is copied once, by
    /// [`Shard::read`], after the comparison has picked it.
    ///
    /// [`MAX_LOG_SHARDS`]: toyos_abi::log::MAX_LOG_SHARDS
    pub fn at_ns(&self, seq: u64) -> Option<u64> {
        if seq < self.oldest_readable() || seq >= self.head() {
            return None;
        }
        let slot = &self.slots[(seq % SHARD_RECORDS as u64) as usize];
        if slot.seq.load(Ordering::Acquire) != seq {
            return None;
        }

        // `at_ns` is the body's first word, which is the whole reason a merge
        // can hold a candidate without holding a record.
        let at_ns = slot.body[0].load(Ordering::Relaxed);

        fence(Ordering::Acquire);
        if slot.seq.load(Ordering::Relaxed) != seq {
            return None;
        }
        Some(at_ns)
    }

    /// Copy record `seq` out, or `None` if this shard cannot answer for it.
    ///
    /// **The window, not just the upper bound.** `head` counts reservations, so
    /// `seq < head` alone admits a number that was handed out and whose record
    /// does not exist yet; and everything below [`Shard::oldest_readable`] has
    /// either been overwritten or was never issued. Both ends are checked
    /// against a word the reader loads anyway.
    ///
    /// Zeroed slots rather than a filled sentinel is what keeps a 512 KiB shard
    /// in `.bss` instead of in the kernel image, and what gives the static and
    /// the allocated shards one initialisation story. [`FIRST_SEQ`] is what
    /// stops the zeroed state colliding with an issued number.
    ///
    /// A stale value can never be mistaken for a live one. Slot `j` only ever
    /// holds numbers congruent to `j` modulo [`SHARD_RECORDS`], the sequence is
    /// a `u64` that never wraps in any reachable lifetime, and the test is exact
    /// equality — so a slot carrying an older generation's number fails against
    /// every `seq` a reader can ask for. That is what kills ABA here.
    pub fn read(&self, seq: u64) -> Option<LogRecord> {
        if seq < self.oldest_readable() || seq >= self.head() {
            return None;
        }
        let slot = &self.slots[(seq % SHARD_RECORDS as u64) as usize];
        if slot.seq.load(Ordering::Acquire) != seq {
            return None;
        }

        // **Word by word, and only as many words as the record claims.** A
        // writer may be storing into these at this very moment — that is what
        // the re-check below is for — and the loads are atomic so that the race
        // is not one: the re-check discards a torn *result*, and only an atomic
        // access makes the read itself defined.
        //
        // `len` came out of a word that may be mid-recycle, so it is clamped
        // before it decides anything. The worst a garbage value can do is make
        // this read the whole message area, which is in bounds and is then
        // thrown away.
        let identity = slot.body[1].load(Ordering::Relaxed);
        let shape = slot.body[2].load(Ordering::Relaxed);
        let len = ((shape >> 16) as u16).min(MSG_BYTES as u16);
        let mut record = LogRecord {
            seq,
            at_ns: slot.body[0].load(Ordering::Relaxed),
            pid: identity as u32,
            tid: (identity >> 32) as u32,
            cpu: shape as u16,
            len,
            elided: (shape >> 32) as u16,
            level: (shape >> 48) as u8,
            flags: (shape >> 56) as u8,
            msg: [0; MAX_RECORD_MESSAGE],
        };
        for i in 0..msg_words(len) {
            let bytes = slot.body[HEADER_WORDS + i].load(Ordering::Relaxed).to_le_bytes();
            record.msg[i * 8..i * 8 + 8].copy_from_slice(&bytes);
        }

        // The re-check is total **because a writer marks the slot before it
        // touches the body**: any writer that started during the copy moved
        // this word to `WRITING` first, so a second load that still answers
        // `seq` means nothing wrote here in between.
        fence(Ordering::Acquire);
        if slot.seq.load(Ordering::Relaxed) != seq {
            return None;
        }

        Some(record)
    }
}

// A `Shard` is `Sync` because every word in it is an `AtomicU64`, and there is
// deliberately no `unsafe impl` here saying so on the type's behalf. Until
// 2026-08-14 there was one, and it stood in for the body's `UnsafeCell` — a
// hand-written claim that a non-atomic write racing a `read_volatile` was
// somebody's problem rather than undefined behaviour. The words are what make
// the claim true, so the compiler makes it instead.

/// Is a reader parked on this machine's records?
///
/// **One bit, and it is what keeps the producer's path free of locked
/// read-modify-writes.** Without it every commit would pay `claim_wake`'s CAS,
/// and `specs/issues/hardware/one-rmw-per-log-line-cost-350ms.md` measured what
/// one of those per line costs under TCG: 350 ms of boot. What a producer pays
/// here is a fence and a relaxed load; the five locked operations of the post
/// are paid at most once per park, by whichever producer wins the swap.
#[cfg(not(feature = "loom"))]
static LOG_WAITER: AtomicBool = AtomicBool::new(false);

/// The machine's one flag. `registry.rs`'s arrangement and its reason: loom's
/// atomics have no `const` constructor, so the model builds its own and hands
/// it to the two functions below.
#[cfg(not(feature = "loom"))]
pub fn log_waiter() -> &'static AtomicBool {
    &LOG_WAITER
}

#[cfg(feature = "loom")]
pub fn waiter() -> AtomicBool {
    AtomicBool::new(false)
}

/// Read the waiter flag **after** the guarded commit store. `true` means "a
/// reader is parked and this caller owns the post".
///
/// The caller does the post and never the ordering, which is the whole reason
/// this lives here rather than in `mod.rs` with `emit`: `shard.rs` is the file
/// `kernel-loom` compiles a second time, and an edge in a file no model reaches
/// is an edge nothing checks. x86 TSO hides a missing one from every guest test.
///
/// The swap admits exactly one poster per park. It is an RMW and it is on the
/// producer's path — but only on the path of the one producer that found the
/// flag set, which is once per park rather than once per record.
pub fn signal_after_commit(waiter: &AtomicBool) -> bool {
    // **Load-bearing, and x86 cannot fail without it.** This half is a store
    // (the commit) followed by a load (the flag), which is the one reordering
    // TSO permits; the waiter's half is the mirror image. Drop either fence and
    // both sides can miss, leaving a committed record under a parked reader —
    // a machine that has gone quiet with something left to say.
    #[cfg(not(feature = "wake-fence-off"))]
    fence(Ordering::SeqCst);
    if !waiter.load(Ordering::Relaxed) {
        return false;
    }
    waiter.swap(false, Ordering::AcqRel)
}

/// Arm the flag, fence, and re-scan. `true` means "do not park".
///
/// **The rescan asks whether a record is committed and never whether `head`
/// moved**, and that is a liveness property rather than a taste: `head` counts
/// reservations, so `head > next` can mean a writer is inside the bounded
/// publication window and busy-waiting on it spends a CPU until the copy
/// finishes. The predicate is the one `drain_ordered` uses, and the caller may
/// then park: a writer still inside its window wakes the waiter with its own
/// post-commit signal, and one that already committed is caught here.
pub fn arm_waiter(waiter: &AtomicBool, committed_record_waiting: impl Fn() -> bool) -> bool {
    waiter.store(true, Ordering::Relaxed);
    #[cfg(not(feature = "wake-fence-off"))]
    fence(Ordering::SeqCst);
    committed_record_waiting()
}
