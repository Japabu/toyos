//! Reading the kernel's log, for the three programs endowed to.
//!
//! The kernel keeps no per-reader state, so all a reader is is a cursor and a
//! buffer. Two readers see the same records and neither consumes the stream.

use toyos_abi::log::{LogCursor, LogRecord};
use toyos_abi::syscall::{self, SyscallError};

use crate::syscap::SysCap;
use crate::AsHandle;

/// One reader's position in the machine's log.
///
/// A fresh tail starts at the oldest record every shard still holds, which is
/// the whole boot on a machine that has not logged 512 records on any CPU yet —
/// so `/bin/logd` starting late still writes this boot's log from its first
/// line.
pub struct LogTail {
    cursor: LogCursor,
}

impl Default for LogTail {
    fn default() -> Self {
        Self::new()
    }
}

impl LogTail {
    pub const fn new() -> Self {
        Self { cursor: LogCursor::new() }
    }

    /// Records this cursor never saw because a producer overwrote them.
    ///
    /// Cumulative and exact: the kernel derives it from the two numbers that
    /// have to be right anyway, so it cannot drift from the ring the way a
    /// producer-side counter would.
    pub fn lost(&self) -> u64 {
        self.cursor.lost
    }

    /// Shards the machine has, once a read has answered. Zero before that.
    pub fn shards(&self) -> u32 {
        self.cursor.shards
    }

    /// Tell the kernel how far this reader has made the log **durable** — the
    /// `at_ns` of the newest record now on the device, not merely written.
    ///
    /// It travels on the next read rather than on a syscall of its own, because
    /// the one caller reads every loop anyway. What it buys is a panicking
    /// kernel that can wait for its own report to reach `/log` instead of
    /// guessing; the kernel clamps it, so a wrong value here shortens that wait
    /// and can never lengthen it.
    pub fn publish_durable(&mut self, at_ns: u64) {
        self.cursor.durable = self.cursor.durable.max(at_ns);
    }

    /// Fill as much of `out` as there is, oldest first, merged by timestamp.
    ///
    /// Answers an empty slice when there is nothing new; **it does not block**,
    /// so a caller with nothing to do arms on a readiness source and parks.
    pub fn read<'a>(
        &mut self,
        cap: &SysCap,
        out: &'a mut [LogRecord],
    ) -> Result<&'a [LogRecord], SyscallError> {
        let count = syscall::log_read(cap.as_handle(), &mut self.cursor, out)?;
        Ok(&out[..count])
    }
}
