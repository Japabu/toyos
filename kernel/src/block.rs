use crate::mm::PAGE_SIZE;
use crate::time::{Budget, Deadline, Duration};

/// Unique identifier for a block device, used as page cache key.
pub type DeviceId = u32;

/// How long one operation on a block device may spend inside the device before
/// it is refused.
///
/// **The number is the caller's and not the driver's, and that split is the
/// whole point.** A driver's own bound covers *one* device round trip —
/// `USB_TIMEOUT_NS` is 2 s in `drivers/xhci` — and says nothing about the
/// composition above it: one `read_blocks` of N blocks is `ceil(N / 8)` SCSI
/// commands, each of which may be issued three times with a Reset Recovery
/// between the attempts, and each of those is three phases with a bound of its
/// own. So a device that answers *every* transfer, just slowly, holds a caller
/// for as long as the work takes and there is nothing above the driver that
/// says how long that may be. This is that number.
///
/// **It is what makes a shipped daemon's give-up policy reachable**, which is
/// the reason it exists rather than a consequence of it. `/bin/logd`'s
/// `LOG_WRITE_BUDGET` is 5 s and it is measured in userland *around the
/// syscall*: a syscall that has not returned cannot be given up on, so every
/// bound below it is what decides whether that policy runs at all. Its own doc
/// used to name `USB_TIMEOUT_NS` as the thing that turns a stick that stopped
/// answering into an `Err` — true of a dead device and never true of a slow
/// one, because that bound is never reached by a device that answers.
///
/// **2 s, and the derivation is two terms.** Below: one whole
/// `USB_TIMEOUT_NS`, so a caller that has spent more than a single transfer's
/// entire allowance on commands that are *completing* is talking to a device
/// too slow to serve, and no healthy device can reach it. Above: the refusal is
/// taken between commands and never inside one, so the overshoot is the command
/// in flight — one more transfer bound at worst — and `2 + 2` leaves a second
/// of the daemon's 5 s for it to notice with.
///
/// **A [`Budget`] and not a [`crate::time::Tripwire`]**: expiry is a degraded
/// answer, named. The operation is refused, the device is *not* marked failed —
/// nothing was in flight when the refusal was taken — and the caller gets the
/// `Err` every other refused transfer produces.
pub const OPERATION: Budget = Budget::of(
    Duration::from_secs(2),
    "the block-device operation is refused with an I/O error, and the caller's \
     own give-up policy decides what happens next",
);

/// When an operation starting now must stop spending device time.
///
/// Minted by the [`BlockDevice`] implementation, which is the layer that knows
/// one call is one operation, and honoured by the driver below it. **A
/// [`Deadline`] because it is absolute**: it crosses into a driver that loops,
/// and a relative duration re-based at each command would bound every command
/// instead of the operation.
pub fn operation_deadline() -> Deadline {
    Deadline::at(crate::clock::now() + OPERATION.duration())
}

/// A transfer the device did not complete.
///
/// Deliberately not an enum. Above this trait there is exactly one thing to do
/// with the answer — stop, and do not believe the buffer — while *why* it
/// failed (which endpoint stalled, what the sense key was, whether the device
/// answered at all) is in the driver's own log line, where it can be read. An
/// enum here would be a vocabulary nothing matches on and every new driver
/// would have to guess an arm from.
///
/// It is not [`SyscallError`] because a driver has no business naming a
/// syscall's return, and because this type is one bit where that one is a
/// vocabulary. The conversion happens where the two meet: `vfs::FileSystem`
/// answers `SyscallError` and [`SyscallError::Io`] is the variant that exists
/// for this, so a refused transfer reaches userland as itself rather than as
/// "no such file".
///
/// [`SyscallError`]: toyos_abi::syscall::SyscallError
/// [`SyscallError::Io`]: toyos_abi::syscall::SyscallError::Io
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockError;

pub type BlockResult = Result<(), BlockError>;

/// Block-oriented storage device interface.
///
/// All I/O is in whole 4KB blocks. No byte-level addressing — that's the
/// filesystem's job. The page cache sits between the filesystem and this trait.
///
/// Every method is fallible because every implementation is: an NVMe command
/// carries a status, and a USB stick can stall, refuse, or be pulled out
/// mid-transfer. When these returned `()` the NVMe driver discarded six
/// completion statuses and the page cache filled a slot from a read that had
/// not happened — which is worse than losing the data, because the slot was
/// already labelled with the new block's number and the *previous tenant's*
/// bytes were then served under it.
pub trait BlockDevice: Send {
    fn device_id(&self) -> DeviceId;
    fn block_count(&self) -> u64;

    /// Read `count` contiguous blocks starting at `lba` into `buf`.
    /// `buf.len()` must equal `count as usize * 4096`.
    ///
    /// On `Err` the contents of `buf` are whatever they were before the call.
    #[must_use = "a failed read leaves the buffer holding whatever it held before"]
    fn read_blocks(&mut self, lba: u64, count: u32, buf: &mut [u8]) -> BlockResult;

    /// Write `count` contiguous blocks starting at `lba` from `buf`.
    /// `buf.len()` must equal `count as usize * 4096`.
    #[must_use = "a failed write did not reach the device"]
    fn write_blocks(&mut self, lba: u64, count: u32, buf: &[u8]) -> BlockResult;

    /// Flush any hardware write caches to persistent storage.
    #[must_use = "a failed flush means the writes before it are not durable"]
    fn flush(&mut self) -> BlockResult;
}

// How much of RAM the two caches above this trait may hold, in 4 KiB pages.
//
// Both numbers are hard ceilings, not targets. Linux lets its page cache take
// the whole machine because it has a pressure signal and a reclaim path to
// give it back on demand; ToyOS has neither (`issues/isolation/no-physical-memory-fairness.md` ("No physical
// memory fairness"), so a cache that grows to fit the workload is a cache
// that starves userland with no way to stop it. Until there is a pressure
// signal, the ceiling has to be a number the machine can lose outright.
//
// The `test-small-caches` overrides exist because the honest ceilings are
// tens of megabytes: a test that reached them by doing real I/O would spend
// minutes proving what 256 KiB proves in a second. The eviction code they
// drive is the shipped code — only the bound moves.

/// Blocks the filesystem metadata cache may hold.
///
/// Metadata residency is a property of the filesystem, not of the machine:
/// formatting the T14's 244 GB namespace leaves ~1900 blocks resident (the
/// number `nvme_large_device` writes back at shutdown), and a mounted
/// filesystem touches far fewer. 4096 blocks is 16 MiB — a little over 2x
/// that peak — so the steady state never evicts, and a cold walk of a btree
/// bigger than the cache degrades to re-reads instead of growing forever.
///
/// RAM enters only as a floor for machines too small to spare 16 MiB, where
/// the filesystem's appetite stops being the binding constraint. It must also
/// stay under 14,336 or the hashbrown index crosses the 16,384-bucket bound
/// `nvme_large_device` asserts.
pub fn metadata_cache_blocks() -> usize {
    if crate::actuator::test_small_caches() {
        return 64;
    }
    let (total, _) = crate::mm::pmm::stats();
    (((total / 32) / PAGE_SIZE) as usize).clamp(64, 4096)
}

/// Pages the file data cache may hold.
///
/// This one *is* a fraction of RAM: unlike metadata, the hot file set is a
/// property of what userland is doing, and there is no smaller number that is
/// right for both a 512 MiB box and a 32 GiB laptop. 1/64 of usable RAM is
/// 64 MiB on the 4 GiB test guest and 256 MiB at the upper clamp — small
/// enough that losing all of it is invisible, large enough to hold every
/// binary the system boots.
pub fn file_cache_pages() -> usize {
    if crate::actuator::test_small_caches() {
        return 64;
    }
    let (total, _) = crate::mm::pmm::stats();
    (((total / 64) / PAGE_SIZE) as usize).clamp(2048, 65536)
}
