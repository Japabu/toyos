//! The kernel's log record, and the cursor that reads it.
//!
//! One layout, two types over it. The kernel's slot is this struct with its
//! first word made atomic; [`LogRecord`] is what a reader gets, and by the time
//! it holds one that word is just a sequence number. Making it *one* type with
//! an `AtomicU64` in it — which an earlier draft did — makes the copy-out a
//! transmute of an atomic into a value nobody synchronises on, and gives a
//! userland reader a field named `commit` that commits nothing.
//!
//! Nothing here dispatches: the kernel's implementation of [`SYS_LOG_READ`]
//! arrives with the record ring it reads, and until then the number falls to the
//! syscall dispatch's default and answers `InvalidArgument`, which is what an
//! unassigned number answers.
//!
//! [`SYS_LOG_READ`]: crate::syscall::SYS_LOG_READ

/// Message bytes a record carries.
///
/// **Sized to the next power-of-two record that holds the measured maximum
/// line.** Across 12,497 committed T14 boot-log lines, message length after the
/// `[kernel … ] ` prefix measured: min 14, p50 59, p90 111, p99 154, p999 857,
/// max 863. The record's other fields are 32 bytes fixed, so [`RECORD_BYTES`] —
/// a power of two by its own derivation — is 32 plus this constant; 1024 is the
/// smallest power of two past 32 + 863, which makes this 992, covering the
/// measured maximum with headroom at zero alignment padding. The unbounded case
/// — a demangled backtrace symbol — is not solved by any fixed bound and is
/// handled separately by head-and-tail elision at the producer
/// (`kernel/src/log/elide.rs`).
pub const MAX_RECORD_MESSAGE: usize = 992;

/// One record on the wire, and one slot in a shard. A power of two so a reader
/// indexes by shift and the kernel never does length arithmetic.
pub const RECORD_BYTES: usize = 1024;

/// Shards a cursor can name, which is the machine's CPU count.
///
/// Not read from `sched::MAX_CPUS`: this is an ABI struct's width, so it is
/// fixed by the ABI and the kernel is what must agree with it. A machine with
/// more CPUs than this is a kernel that cannot answer [`SYS_LOG_READ`] at all,
/// which is a build-time disagreement rather than a runtime one.
pub const MAX_LOG_SHARDS: usize = 8;

/// What a record is, to the one consumer that treats them differently.
///
/// **Three variants because three have callers today.** A finer set is a level
/// with no reader, which is a field built for a plan. Nothing orders these and
/// every consumer matches exhaustively — this is not a severity ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Level {
    /// `log!`. Everything reads it.
    Info = 0,
    /// `boot_phase!`. The panel repaints on one.
    Phase = 1,
    /// `alert!`. The panel paints the row red.
    ///
    /// **This deletes a magic-value sentinel.** `panic_console::has_alert` scans
    /// each row for three consecutive `!` bytes and its own comment enumerates
    /// the strings that happen to match, which is the comment root `CLAUDE.md`
    /// says is the type you should have written.
    Alert = 2,
}

impl Level {
    /// A byte that crossed the syscall boundary, or came out of a persisted
    /// region, is not a `Level` until this says so.
    pub const fn from_u8(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Info),
            1 => Some(Self::Phase),
            2 => Some(Self::Alert),
            _ => None,
        }
    }
}

/// Set when the record was written before this CPU's per-CPU area was ready.
///
/// The `boot` label today's prefix carries, as a bit: cpu0's shard *is* the boot
/// shard, so there is no handoff and the renderer prints the same word.
pub const FLAG_EARLY: u8 = 1 << 0;

/// What a reader gets. Plain POD, `Copy`, no interior mutability.
#[repr(C, align(64))]
#[derive(Clone, Copy)]
pub struct LogRecord {
    /// The record's identity. In the kernel's slot this same word is also the
    /// validity word; by the time a reader holds a copy it is just the sequence
    /// number, and it is what [`LogCursor::next`] counts in.
    pub seq: u64,
    pub at_ns: u64,
    pub pid: u32,
    pub tid: u32,
    pub cpu: u16,
    /// Message bytes present in `msg`, never above [`MAX_RECORD_MESSAGE`].
    pub len: u16,
    /// Bytes the message would have had past [`MAX_RECORD_MESSAGE`],
    /// saturating. **Never a silent truncation** — this is the difference
    /// between a bound and a lie.
    pub elided: u16,
    pub level: u8,
    /// [`FLAG_EARLY`] and nothing else yet.
    pub flags: u8,
    pub msg: [u8; MAX_RECORD_MESSAGE],
}

const _: () = assert!(core::mem::size_of::<LogRecord>() == RECORD_BYTES);
const _: () = assert!(core::mem::align_of::<LogRecord>() == 64);
/// Every byte belongs to a field: this crosses the boundary through
/// [`LogRecord::as_bytes`], so a gap would publish whatever the kernel stack
/// held. Spelled as the sum of the field widths rather than as
/// [`RECORD_BYTES`], which is the *other* claim about this struct — a padded
/// layout that happened to reach 1024 bytes would satisfy that one.
const _: () = assert!(
    core::mem::size_of::<LogRecord>() == 8 + 8 + 4 + 4 + 2 + 2 + 2 + 1 + 1 + MAX_RECORD_MESSAGE
);
/// The kernel's slot is this layout with the first word made atomic, so the
/// body it copies is everything past that word and must start where it does.
const _: () = assert!(core::mem::offset_of!(LogRecord, at_ns) == core::mem::size_of::<u64>());

impl LogRecord {
    /// A record no shard ever wrote, for sizing a read buffer.
    ///
    /// Not `Default`, and the reason is the state this is indistinguishable
    /// from: an all-zero record is exactly a **zeroed slot** — a shard's `.bss`
    /// or `alloc_zeroed` storage that nothing has ever written. Sequence
    /// numbers start at *one* (`FIRST_SEQ`) precisely so that state can never be
    /// read as a record; a type whose `Default` produced it would hand that
    /// state back through the front door. The name says it is filler.
    pub const EMPTY: Self = Self {
        seq: 0,
        at_ns: 0,
        pid: 0,
        tid: 0,
        cpu: 0,
        len: 0,
        elided: 0,
        level: Level::Info as u8,
        flags: 0,
        msg: [0; MAX_RECORD_MESSAGE],
    };

    /// The message, as far as it is text.
    ///
    /// **A record crossed the syscall boundary, so its bytes are input.** `len`
    /// is clamped rather than trusted and a non-UTF-8 body answers with what
    /// decoded, because a diagnostic that refuses to render a corrupt record is
    /// a diagnostic that hides the corruption it was called to show.
    pub fn message(&self) -> &str {
        let len = (self.len as usize).min(MAX_RECORD_MESSAGE);
        let bytes = &self.msg[..len];
        match core::str::from_utf8(bytes) {
            Ok(text) => text,
            Err(e) => {
                // `from_utf8` guarantees the prefix before `valid_up_to` is
                // valid, so this cannot fail and is not an `expect` on input.
                core::str::from_utf8(&bytes[..e.valid_up_to()]).unwrap_or("")
            }
        }
    }

    /// The record's own bytes, which is what goes on the wire.
    ///
    /// The shape its six siblings in this crate have (`NicInfo`,
    /// `VirtioSoundInfo`, `FramebufferInfo`, `RawKeyEvent`, `MouseEvent`,
    /// `HdaInfo`), and it is here rather than at the kernel's copy-out for the
    /// reason they are: the `unsafe` belongs beside the layout assertion that
    /// discharges it, not beside the caller that happens to need it.
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: `self` is a valid `&Self` (non-null, aligned, readable for
        // `size_of::<Self>()` bytes), and the const assert above proves the
        // `repr(C)` layout has no padding, so every byte the slice exposes is
        // an initialized field, not a gap.
        unsafe {
            core::slice::from_raw_parts(self as *const Self as *const u8, core::mem::size_of::<Self>())
        }
    }

    pub fn level(&self) -> Option<Level> {
        Level::from_u8(self.level)
    }

    pub fn is_early(&self) -> bool {
        self.flags & FLAG_EARLY != 0
    }
}

/// One implementation of a rendered line, so the kernel's serial sink, the
/// panel, `logd` and any diagnostic tool produce byte-identical text.
///
/// It renders the *body* — timestamp, origin and message — and no prefix of its
/// own, because the three callers disagree about the prefix on purpose: `logd`
/// writes a wall clock into `/log`, the panel writes a monotonic offset into 80
/// columns, and both are the same record.
impl core::fmt::Display for LogRecord {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let secs = self.at_ns / 1_000_000_000;
        let millis = self.at_ns % 1_000_000_000 / 1_000_000;
        write!(f, "[{secs}.{millis:03} cpu{}", self.cpu)?;
        if self.is_early() {
            f.write_str(" boot")?;
        }
        if self.tid != 0 {
            write!(f, " tid={}", self.tid)?;
        }
        f.write_str("] ")?;
        f.write_str(self.message())?;
        if self.elided != 0 {
            write!(f, " …[{} bytes elided]", self.elided)?;
        }
        Ok(())
    }
}

/// Per-reader state. **The kernel holds none.**
///
/// No object, no handle lifecycle, no cursor to leak or go stale, and a second
/// reader costs nothing. The stream is not consumed either: `logd` and a
/// `log-follow` tool coexist with no coordination.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LogCursor {
    /// Out: how many shards the machine has. A caller passes a zeroed cursor
    /// the first time and reads it back.
    pub shards: u32,
    pub _pad: u32,
    /// In/out: cumulative records this cursor never saw because they were
    /// overwritten.
    ///
    /// **Derived, never counted by a producer.** The kernel computes it from
    /// `head` and `next`, which both have to be right anyway, so no counter can
    /// drift from the ring. It lives here so a reader that ignores loss has to
    /// actively ignore a field it is already passing.
    pub lost: u64,
    /// In: the timestamp of the newest record the caller has made durable, or
    /// zero. The kernel takes the maximum, **after clamping it to the newest
    /// record it actually holds**: this is a number that crossed the trust
    /// boundary and decides how long a dying kernel waits for its own report,
    /// and an unclamped `u64::MAX` from a buggy `logd` would lose it silently.
    /// Clamping cannot lengthen the wait, so the worst a hostile writer does is
    /// shorten one for its own output.
    pub durable: u64,
    /// In/out: the next sequence number wanted from each shard.
    pub next: [u64; MAX_LOG_SHARDS],
}

const _: () = assert!(core::mem::size_of::<LogCursor>() == 24 + 8 * MAX_LOG_SHARDS);

impl LogCursor {
    /// A cursor that has read nothing. The kernel fills `shards` on the first
    /// call.
    pub const fn new() -> Self {
        Self { shards: 0, _pad: 0, lost: 0, durable: 0, next: [0; MAX_LOG_SHARDS] }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::string::ToString;

    /// The layout the kernel's slot is the same eight bytes of, and the one a
    /// persisted region is a byte-for-byte array of. A change here is a change
    /// to both and the `const` assertions above are what say so.
    #[test]
    fn the_record_is_the_size_and_shape_both_sides_assume() {
        assert_eq!(core::mem::size_of::<LogRecord>(), RECORD_BYTES);
        assert_eq!(core::mem::align_of::<LogRecord>(), 64);
        assert_eq!(core::mem::offset_of!(LogRecord, seq), 0);
        assert_eq!(core::mem::offset_of!(LogRecord, at_ns), 8);
        assert_eq!(core::mem::size_of::<LogCursor>(), 88);
    }

    /// **The encoder is the wire, so the test decodes the wire.**
    ///
    /// Not `as_bytes().len() == RECORD_BYTES`, which a padded struct passes:
    /// every field is read back out of the slice at the offset `#[repr(C)]`
    /// puts it at, and the tail is the message. A gap anywhere before `msg`
    /// shifts one of these and the assertion that catches it is the one whose
    /// field moved.
    #[test]
    fn as_bytes_is_the_fields_and_nothing_between_them() {
        let r = record("hello");
        let b = r.as_bytes();
        assert_eq!(b.len(), RECORD_BYTES);
        assert_eq!(u64::from_ne_bytes(b[0..8].try_into().unwrap()), 7);
        assert_eq!(u64::from_ne_bytes(b[8..16].try_into().unwrap()), 1_234_567_890);
        assert_eq!(u32::from_ne_bytes(b[16..20].try_into().unwrap()), 3);
        assert_eq!(u32::from_ne_bytes(b[20..24].try_into().unwrap()), 4);
        assert_eq!(u16::from_ne_bytes(b[24..26].try_into().unwrap()), 2);
        assert_eq!(u16::from_ne_bytes(b[26..28].try_into().unwrap()), 5);
        assert_eq!(u16::from_ne_bytes(b[28..30].try_into().unwrap()), 0);
        assert_eq!(b[30], Level::Info as u8);
        assert_eq!(b[31], 0);
        assert_eq!(&b[32..37], b"hello");
        assert!(b[37..].iter().all(|&x| x == 0));
    }

    fn record(msg: &str) -> LogRecord {
        let mut r = LogRecord {
            seq: 7,
            at_ns: 1_234_567_890,
            pid: 3,
            tid: 4,
            cpu: 2,
            len: msg.len() as u16,
            elided: 0,
            level: Level::Info as u8,
            flags: 0,
            msg: [0; MAX_RECORD_MESSAGE],
        };
        r.msg[..msg.len()].copy_from_slice(msg.as_bytes());
        r
    }

    #[test]
    fn a_record_renders_the_same_line_for_every_consumer() {
        assert_eq!(record("hello").to_string(), "[1.234 cpu2 tid=4] hello");
    }

    /// The two decorations, each of which a consumer would otherwise invent.
    #[test]
    fn early_and_elided_are_in_the_line_rather_than_in_a_convention() {
        let mut r = record("x");
        r.flags = FLAG_EARLY;
        r.tid = 0;
        assert_eq!(r.to_string(), "[1.234 cpu2 boot] x");

        let mut r = record("x");
        r.elided = 900;
        assert_eq!(r.to_string(), "[1.234 cpu2 tid=4] x …[900 bytes elided]");
    }

    /// **`len` came across the syscall boundary**, so a record claiming more
    /// message than a record can hold answers with what it has rather than
    /// panicking a reader. `logd` is userland and this is its input too.
    #[test]
    fn a_length_past_the_bound_is_clamped_and_not_a_panic() {
        let mut r = record("abc");
        r.len = u16::MAX;
        assert_eq!(r.message().len(), MAX_RECORD_MESSAGE);
    }

    /// A corrupt tail must not hide the readable head: a diagnostic that
    /// refuses to render a broken record hides the breakage it exists to show.
    #[test]
    fn a_non_utf8_body_renders_what_decoded() {
        let mut r = record("ok");
        r.msg[2] = 0xff;
        r.len = 3;
        assert_eq!(r.message(), "ok");
    }

    /// A `u8` from the wire is not a `Level` until [`Level::from_u8`] says so —
    /// there is no `unsafe` transmute anywhere on this path.
    #[test]
    fn an_undeclared_level_byte_decodes_to_nothing() {
        assert_eq!(Level::from_u8(2), Some(Level::Alert));
        assert_eq!(Level::from_u8(3), None);
        assert_eq!(record("x").level(), Some(Level::Info));
    }

    #[test]
    fn a_fresh_cursor_has_read_nothing() {
        assert_eq!(LogCursor::new(), LogCursor::default());
        assert_eq!(LogCursor::new().next, [0; MAX_LOG_SHARDS]);
    }
}
