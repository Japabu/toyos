//! `SYS_LOG_READ`, and the readiness source a reader with nothing to read arms
//! on.
//!
//! **The kernel keeps no per-reader state.** A cursor is a caller's own eight
//! sequence numbers and a loss count, in the caller's own memory; this file
//! copies it in, walks [`drain_ordered`] with it, copies whole records out and
//! copies the cursor back. There is no object, no handle lifecycle, nothing to
//! leak or go stale, and a second reader costs nothing — the stream is not
//! consumed, so `/bin/logd` and a `log-follow` tool coexist with no
//! coordination.
//!
//! **Reading the whole machine's log is authority**, so it rides
//! [`Rights::LOG`] on a `SysCap` rather than being ambient: every record every
//! CPU wrote is every process's business and no process's right by default.

use alloc::vec::Vec;

use toyos_abi::log::{LogCursor, LogRecord, RECORD_BYTES};
use toyos_abi::syscall::SyscallError;

use crate::inbox::InboxId;
use crate::sync::Lock;
use crate::user_ptr::UserBytesMut;

use super::read::{drain_ordered, Cursor, RecordSink};

/// Rings with a `POLL_ADD` outstanding on the machine's log.
///
/// **A sixth per-source watcher list, knowingly.** `keyboard`, `mouse`, `net`,
/// `virtio_sound` and `hda` each carry one of exactly this shape, and the
/// completion architecture's C3 folds all six into one watch list and deletes
/// them together. Adding a sixth instance of a mechanism that is about to be
/// unified is the honest cost of landing first, and it is one static and one
/// match arm.
static INBOX_WATCHERS: Lock<Vec<InboxId>> = Lock::new(Vec::new());

pub fn add_inbox_watcher(id: InboxId) {
    let mut w = INBOX_WATCHERS.lock();
    if !w.contains(&id) {
        w.push(id);
    }
}

pub fn remove_inbox_watcher(id: InboxId) {
    INBOX_WATCHERS.lock().retain(|&x| x != id);
}

pub fn inbox_watchers() -> Vec<InboxId> {
    INBOX_WATCHERS.lock().clone()
}

/// Tell every ring watching the log that records have moved.
///
/// **Posted by `klogd` after each drain batch, and deliberately not by
/// `emit`.** The list is a `Lock<Vec<InboxId>>` and the post clones it under the
/// lock, which is the one thing `emit` may not do — it runs inside `sync.rs`,
/// inside IRQ handlers, inside the scheduler and inside every syscall's locked
/// region. `klogd` is the context that has just observed committed records and
/// may take a lock, and posting there costs one wake per batch rather than one
/// per record (§2.6a's argument, applied to the second consumer).
///
/// **The readiness is an edge and not a level**, because a level is a question
/// the kernel cannot answer: "is there anything for *you*" is a property of a
/// cursor the kernel does not hold. So a reader closes the window itself, in
/// the shape `shard::arm_waiter` already uses on the kernel's side — submit the
/// poll, read once more, and park only if that read was empty.
pub fn post_readiness() {
    let watchers = inbox_watchers();
    if watchers.is_empty() {
        return;
    }
    crate::inbox::complete_pending_for_event(&watchers, crate::inbox::Source::Log);
}

/// Records into a caller's buffer, at [`RECORD_BYTES`] stride.
///
/// **Whole records at a fixed stride, never packed.** The kernel does no length
/// arithmetic and the caller indexes by shift; at the measured 100.2-byte mean
/// payload the waste is nine tenths of what moves, and it is still the right
/// trade against putting "is this record whole?" back into every reader.
struct UserRecords<'a, 'b> {
    out: &'a mut UserBytesMut<'b>,
    written: usize,
    capacity: usize,
}

impl RecordSink for UserRecords<'_, '_> {
    fn put(&mut self, record: &LogRecord) -> bool {
        if self.written >= self.capacity {
            return false;
        }
        // SAFETY: a `LogRecord` is `#[repr(C)]`, `Copy` and valid for any bit
        // pattern, so its own bytes are what goes on the wire; `RECORD_BYTES`
        // is `size_of::<LogRecord>()`, asserted in `toyos-abi`'s own
        // `const _`. The slice is over one whole record the kernel built on
        // its own stack — it borrows nothing of the shard and nothing of
        // userland — and it dies inside this call.
        //
        // **Irreducible here and not irreducible.** Six other types that cross
        // this boundary carry a safe `as_bytes` whose one block lives in
        // `toyos-abi` beside the layout it depends on — `NicInfo`,
        // `VirtioSoundInfo`, `FramebufferInfo`, `RawKeyEvent`, `MouseEvent`,
        // `HdaInfo`; `LogRecord` is the one that does not, so the kernel
        // spells the same thing by hand.
        // Giving it one is an edit to `toyos-abi/src`, which lands on its own
        // pull request and costs a sysroot claim — filed as
        // `issues/build/logrecord-has-no-as-bytes.md` rather than smuggled
        // into a sweep.
        let bytes = unsafe {
            core::slice::from_raw_parts((record as *const LogRecord).cast::<u8>(), RECORD_BYTES)
        };
        self.out.write_at(self.written * RECORD_BYTES, bytes);
        self.written += 1;
        true
    }
}

/// Fill `out` with the records `cursor` has not seen, oldest first, merged by
/// `at_ns`. Answers how many records were written.
///
/// **It never blocks**: nothing new is `0`, and a caller with nothing to do
/// arms on the readiness source above and parks. A syscall that waited would be
/// a second blocking mechanism in a kernel that is converging on one.
pub fn read(
    cursor: &mut LogCursor,
    out: &mut UserBytesMut,
    capacity: usize,
) -> Result<usize, SyscallError> {
    // **The storm starts with the first reader and not at boot**, because a
    // storm nobody is reading has spent itself before the gate opens a cursor.
    // `storm::start_once` is idempotent and costs one relaxed swap after that.
    #[cfg(feature = "boot-actuators")]
    if crate::actuator::log_storm() {
        super::storm::start_once();
    }
    // The nesting gate, armed here for the same reason and once — on a kernel
    // thread of its own, because `IF` is clear for the whole of every syscall
    // and a record emitted from one is bracketed whether the guard exists or
    // not. One thread for both windows: which of the two the injection aims at
    // is `log::nested`'s to decide from the arm set, and a boot arming neither
    // spawns nothing.
    #[cfg(feature = "boot-actuators")]
    if crate::actuator::log_nested_emit() || crate::actuator::log_nested_reserve() {
        super::nested::start_once();
    }

    let shards = super::shard_count();
    // **Refused, never truncated.** A buffer that cannot hold one record has
    // nowhere to put an answer, and one that cannot hold a record per shard
    // cannot carry what a single call may have to merge. Both bounds are
    // knowable before the first call — `MAX_LOG_SHARDS` is an ABI constant and
    // is always enough — so no caller has to learn either from a refusal.
    if capacity == 0 || capacity < shards as usize {
        return Err(SyscallError::InvalidArgument);
    }

    let mut walk = Cursor::from_reader(cursor);
    let mut sink = UserRecords { out, written: 0, capacity };
    drain_ordered(&mut walk, &mut sink);
    let written = sink.written;

    walk.write_into(cursor);
    // The one field the kernel writes without being asked: a caller passes a
    // zeroed cursor the first time and reads back how many shards it is
    // reading.
    cursor.shards = shards;
    // `durable` is the caller's word to the kernel and travels the other way.
    // It is left exactly as the caller wrote it rather than zeroed, so a reader
    // that publishes into a cursor it keeps does not have to re-publish every
    // call.
    publish_durable(cursor.durable);
    Ok(written)
}

/// How far `/bin/logd` says the machine's log is on the device, as the `at_ns`
/// of the newest record it has `fsync`ed.
///
/// **The one number userland tells the kernel about the log**, and the only
/// thing that reads it is a machine that is stopping: `apic::wait_for_log_file`
/// on the panic path and `SYS_SHUTDOWN` on the way to the power-off, each
/// waiting, bounded, for its own last records to reach the stick. Monotone
/// because it is a maximum: a reader that goes away, is killed, or publishes a
/// cursor it has kept since before its last write can never move it backwards
/// and make the kernel wait for something that has already landed.
///
/// Zero until the first publication, which reads as "nothing is durable" and is
/// the honest state of a machine with no `/log`, a `logd` that has not run yet
/// or one that has given up on the volume. Each of those pays the bound on a
/// fatal panic, once, and that is the correct outcome rather than a cost: the
/// report is not on the stick, so there is nothing to return early for.
static DURABLE_NS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Take the caller's claim, clamped, and keep the maximum.
///
/// **`durable` crossed the trust boundary and decides how long a dying kernel
/// waits, so it is clamped** (§6.4). The ceiling is the newest record the
/// machine actually holds: an unclamped `u64::MAX` from a buggy `logd` makes
/// the wait return immediately and the report is silently lost, which is
/// exactly the "a device's own numbers are untrusted" rule one layer up.
///
/// Clamping cannot make a wait *longer* than its own bound, so the only thing a
/// hostile `logd` can do with this is shorten a wait for its own output, which
/// is acceptable and is stated so nobody reads the clamp as more than it is.
fn publish_durable(claimed: u64) {
    if claimed == 0 {
        return;
    }
    let clamped = claimed.min(super::read::newest_committed_at_ns());
    DURABLE_NS.fetch_max(clamped, core::sync::atomic::Ordering::Relaxed);
}

/// The newest record `/bin/logd` has put on the device. One relaxed load, for
/// the two callers that ask it from a machine on its way down.
pub fn durable_ns() -> u64 {
    DURABLE_NS.load(core::sync::atomic::Ordering::Relaxed)
}

// **There was a `pub fn owed() -> bool` here and it is deleted rather than
// wired up.** Its doc called it "the predicate both waits are written against,
// in one place", and neither wait called it: `apic::owed` and
// `log::wait_for_durable` each snapshot `newest_committed_at_ns()` *once*, as
// `want`, and then wait for `durable_ns()` to reach that. This one re-read the
// newest record on every call, which is a different question and a worse one —
// a machine still committing records while it shuts down would never satisfy
// it, so calling it from either site would have turned a bounded wait into one
// that always pays its whole ceiling. Dead code with a claim in it about two
// callers it did not have.
