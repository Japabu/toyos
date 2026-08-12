//! One CPU's ring of whole records, and the two operations that make "half a
//! record" untypeable.
//!
//! **This file is compiled a second time by `kernel-loom`**, so it may name
//! only what that crate shims: the cell, the atomics, and
//! `arch::percpu_fetch_add`. That is not a style rule — x86's TSO gives every
//! load acquire and every store release semantics, so a missing edge here is
//! invisible to every guest test, and loom is the only instrument in this tree
//! that can see one. **ARM64 is planned**, and on it the missing edge is not
//! hypothetical. If this file grows a dependency on a subject, the model stops
//! compiling and the ordering stops being checked by anything.
//!
//! `specs/log-architecture-spec.md` §2.2, §2.4 and §2.5.

#[cfg(not(feature = "loom"))]
use core::cell::UnsafeCell;
#[cfg(not(feature = "loom"))]
use core::sync::atomic::{fence, AtomicU64, Ordering};

#[cfg(feature = "loom")]
use crate::cell::UnsafeCell;
#[cfg(feature = "loom")]
use loom::sync::atomic::{fence, AtomicU64, Ordering};

use toyos_abi::log::{LogRecord, MAX_RECORD_MESSAGE};
/// Only the layout assertions name it, and those are the kernel build's.
#[cfg(not(feature = "loom"))]
use toyos_abi::log::RECORD_BYTES;

/// Slots per CPU: 128 KiB, and 1 MiB at the shipped eight.
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

/// The body of a record — [`LogRecord`] minus the word the writer publishes
/// with.
///
/// It is a separate struct so that the one thing a reader may see torn is
/// exactly the thing the sequence number answers for.
#[repr(C)]
#[derive(Clone, Copy)]
struct Body {
    at_ns: u64,
    pid: u32,
    tid: u32,
    cpu: u16,
    len: u16,
    elided: u16,
    level: u8,
    flags: u8,
    msg: [u8; MAX_RECORD_MESSAGE],
}

#[cfg(not(feature = "loom"))]
const _: () = assert!(core::mem::size_of::<Body>() == RECORD_BYTES - core::mem::size_of::<u64>());

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

/// One record's storage. **The same layout as [`LogRecord`] with the first word
/// made atomic**, and nothing else differs.
#[repr(C, align(64))]
pub struct Slot {
    /// The state word: a sequence number, or [`WRITING`], or zero for a slot
    /// nothing has touched. The identity and the validity are the same word, so
    /// there is no separate valid flag that could disagree with it.
    seq: AtomicU64,
    body: UnsafeCell<Body>,
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
            body: UnsafeCell::new(Body {
                at_ns: 0,
                pid: 0,
                tid: 0,
                cpu: 0,
                len: 0,
                elided: 0,
                level: 0,
                flags: 0,
                msg: [0; MAX_RECORD_MESSAGE],
            }),
        };
        Self { head: AtomicU64::new(FIRST_SEQ), slots: [EMPTY; SHARD_RECORDS] }
    }

    /// Loom's atomics have no `const` constructor, and its `UnsafeCell` records
    /// a creation event, so the model builds shards at run time.
    #[cfg(feature = "loom")]
    pub fn new() -> Self {
        Self {
            head: AtomicU64::new(FIRST_SEQ),
            slots: core::array::from_fn(|_| Slot {
                seq: AtomicU64::new(0),
                body: UnsafeCell::new(Body {
                    at_ns: 0,
                    pid: 0,
                    tid: 0,
                    cpu: 0,
                    len: 0,
                    elided: 0,
                    level: 0,
                    flags: 0,
                    msg: [0; MAX_RECORD_MESSAGE],
                }),
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
    /// Keeping this as an in-place constructor avoids materialising a 128 KiB
    /// [`Shard`] on the BSP's stack.
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
        // value both times and accepted the tear. `kernel-loom`'s
        // `a_reader_racing_a_recycle_gets_nothing_rather_than_a_mixture` found
        // it on its first run; no guest test can, on any machine.
        //
        // The release fence is what puts this store ahead of the body writes
        // for the reader, rather than merely ahead of them in this function.
        slot.seq.store(WRITING, Ordering::Relaxed);
        fence(Ordering::Release);

        // SAFETY: the live guard makes this sequence number and slot exclusively
        // ours until we publish it. The slot now reads `WRITING`, so no reader
        // will accept anything it finds here.
        unsafe {
            *slot.body.get() = Body {
                at_ns: record.at_ns,
                pid: record.pid,
                tid: record.tid,
                cpu: record.cpu,
                len: record.len.min(MAX_RECORD_MESSAGE as u16),
                elided: record.elided,
                level: record.level,
                flags: record.flags,
                msg: record.msg,
            };
        }

        // The store that publishes, and it is the last one.
        slot.seq.store(seq, Ordering::Release);
    }

    /// Copy record `seq` out, or `None` if this shard cannot answer for it.
    ///
    /// **The window, not just the upper bound.** `head` counts reservations, so
    /// `seq < head` alone admits a number that was handed out and whose record
    /// does not exist yet; and everything below [`Shard::oldest_readable`] has
    /// either been overwritten or was never issued. Both ends are checked
    /// against a word the reader loads anyway.
    ///
    /// Zeroed slots rather than a filled sentinel is what keeps a 128 KiB shard
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

        // **A volatile byte copy, and that is a requirement rather than a
        // style.** Reading 248 bytes a writer may concurrently be storing into
        // is a data race in Rust's model whatever x86 does about it: the
        // re-check below makes the *result* sound and does not make the
        // *access* defined. Loom does not see this and neither does any guest
        // test.
        //
        // SAFETY: the slot is live for the whole of this read, and nothing here
        // forms a reference to the body.
        let body: Body = unsafe { core::ptr::read_volatile(slot.body.get()) };

        // The re-check is total **because a writer marks the slot before it
        // touches the body**: any writer that started during the copy moved
        // this word to `WRITING` first, so a second load that still answers
        // `seq` means nothing wrote here in between.
        fence(Ordering::Acquire);
        if slot.seq.load(Ordering::Relaxed) != seq {
            return None;
        }

        Some(LogRecord {
            seq,
            at_ns: body.at_ns,
            pid: body.pid,
            tid: body.tid,
            cpu: body.cpu,
            len: body.len,
            elided: body.elided,
            level: body.level,
            flags: body.flags,
            msg: body.msg,
        })
    }
}

// SAFETY: `head` is written only by the owning CPU and read atomically by every
// other; a body is written only by the CPU that reserved its sequence number,
// and read only through `read`, whose exact-equality re-check rejects any body
// a writer has begun to overwrite.
unsafe impl Sync for Shard {}
