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
//!
//! `specs/log-architecture-spec.md` §3.2.

use alloc::vec::Vec;

use toyos_abi::log::{LogCursor, LogRecord, RECORD_BYTES};
use toyos_abi::syscall::SyscallError;

use crate::io_uring::RingId;
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
/// match arm (`specs/log-architecture-spec.md` §3.2).
static IO_URING_WATCHERS: Lock<Vec<RingId>> = Lock::new(Vec::new());

pub fn add_io_uring_watcher(id: RingId) {
    let mut w = IO_URING_WATCHERS.lock();
    if !w.contains(&id) {
        w.push(id);
    }
}

pub fn remove_io_uring_watcher(id: RingId) {
    IO_URING_WATCHERS.lock().retain(|&x| x != id);
}

pub fn io_uring_watchers() -> Vec<RingId> {
    IO_URING_WATCHERS.lock().clone()
}

/// Tell every ring watching the log that records have moved.
///
/// **Posted by `klogd` after each drain batch, and deliberately not by
/// `emit`.** The list is a `Lock<Vec<RingId>>` and the post clones it under the
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
    let watchers = io_uring_watchers();
    if watchers.is_empty() {
        return;
    }
    crate::io_uring::complete_pending_for_event(&watchers, crate::io_uring::Source::Log);
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
        // A `LogRecord` is `#[repr(C)]`, `Copy` and valid for any bit pattern,
        // so its own bytes are what goes on the wire. The slice is over one
        // whole record the kernel built on its own stack — it borrows nothing
        // of the shard and nothing of userland.
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
    // not.
    #[cfg(feature = "boot-actuators")]
    if crate::actuator::log_nested_emit() {
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

    walk.into_reader(cursor);
    // The one field the kernel writes without being asked: a caller passes a
    // zeroed cursor the first time and reads back how many shards it is
    // reading.
    cursor.shards = shards;
    // `durable` is the caller's word to the kernel and travels the other way.
    // Nothing reads it yet: `LOG_DURABLE_NS` and the clamp that guards it are
    // the panic path's (§6.4), and the panic path still waits on the kernel's
    // own file sink until `/bin/logd` replaces it. It is left exactly as the
    // caller wrote it rather than zeroed, so a reader that publishes into a
    // cursor it keeps does not have to re-publish every call.
    Ok(written)
}
