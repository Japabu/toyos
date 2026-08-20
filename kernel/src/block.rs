use crate::mm::PAGE_SIZE;

/// Unique identifier for a block device, used as page cache key.
pub type DeviceId = u32;

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
