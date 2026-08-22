use crate::mm::PAGE_SIZE;
use crate::scheduler::Operation;
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
/// too slow to serve. Above: the refusal is
/// taken between commands and never inside one, so the overshoot is the command
/// in flight — one more transfer bound at worst — and `2 + 2` leaves a second
/// of the daemon's 5 s for it to notice with.
///
/// **What the derivation does not cover, and this doc used to claim it did.**
/// The clock is [`crate::clock::now`], which is the TSC, and a TCG guest's TSC
/// advances with the *host's* real time; [`begin_operation`] is also called
/// above the `XHCI` ticket lock, so lock-wait and any host descheduling of the
/// vCPU thread are charged to this budget. A healthy device therefore can reach
/// it — the same is now recorded one layer down for `USB_TIMEOUT_NS` — and what
/// the caller is told when it does is the sentence below.
///
/// **The term the two-part derivation is missing is the recovery.** "One whole
/// `USB_TIMEOUT_NS`" is the allowance for transfers that *complete*; a transfer
/// that breached its own bound did not, and what the driver does next is a Reset
/// Recovery and a re-issue (`xhci/wait/msc.rs`'s `scsi`, and its own doc on why
/// recovering and then reporting failure loses a write the device would have
/// taken). With this budget equal to that bound, one breached transfer spends
/// all of it and `MAX_TRANSPORT_ATTEMPTS` is unreachable: the recovery succeeds
/// and the re-issue is refused unissued. Measured 2026-08-22, 1 red in 73 full
/// 12-wide suites, `esp_filesystem`'s `fsync` on `/log`; the identical break was
/// absorbed by the retry on CI on 2026-08-13, before this constant existed.
/// Sizing it is a trade against `/bin/logd`'s 5 s on one side and the CPU pin in
/// `issues/audio/disk-wait-pins-a-cpu.md` on the other, so the number is not an
/// agent's to move.
///
/// **A [`Budget`] and not a [`crate::time::Tripwire`]**: expiry is a degraded
/// answer, named. The operation is refused, the device is *not* marked failed —
/// nothing was in flight when the refusal was taken — and the caller is told
/// which of the two happened: [`BlockError::BudgetExpired`], which reaches
/// userland as `SyscallError::WouldBlock` and never as
/// [`SyscallError::Io`](toyos_abi::syscall::SyscallError::Io). Until
/// 2026-08-22 it did not, and `/bin/logd` ended a boot's log for a stick that
/// was answering.
pub const OPERATION: Budget = Budget::of(
    Duration::from_secs(2),
    "the block-device operation is refused as one that would block, and the \
     caller's own give-up policy decides whether to ask again",
);

/// Declare the running context inside one block-device operation, bounded by
/// [`OPERATION`], until the guard drops.
///
/// Established by the [`BlockDevice`] implementation, which is the layer that
/// knows one call is one operation, and *recovered* by the driver below it
/// rather than handed to it: [`Operation`] carries why owner ruling 1B put the
/// deadline on the running context instead of in an argument, and what else
/// rides the same word.
///
/// **A [`Deadline`] because it is absolute**: it crosses into a driver that
/// loops, and a relative duration re-based at each command would bound every
/// command instead of the operation.
#[must_use = "the operation lasts exactly as long as this guard"]
pub fn begin_operation() -> Operation {
    Operation::begin(Deadline::at(crate::clock::now() + OPERATION.duration()))
}

/// An operation this trait did not complete, and which of two reasons it was.
///
/// **Two variants and not one bit, because they are not the same fact and the
/// machine's one consumer acts on them differently.** This was one bit until
/// 2026-08-22, on the ground that "above this trait there is exactly one thing
/// to do with the answer — stop, and do not believe the buffer". That is true
/// of the *buffer* and false of the caller: `/bin/logd` gives its volume up
/// permanently on any `Err` from `SYS_FSYNC`, which is right for a stick that
/// cannot flush and wrong for a refusal that means "your budget, not this
/// stick". `issues/boot-media/fsync-on-log-returns-other-under-a-loaded-host.md`
/// carried the measurement: 1 red in 73 full 12-wide suites, a boot whose peers
/// were up in 1,385 ms spending `syscall_wall=2108ms` inside one `SYS_FSYNC`,
/// and a boot's log ended for a device that was fine.
///
/// It is still not a *vocabulary*. Which endpoint stalled, what the sense key
/// was and whether the device answered at all stay in the driver's own log
/// line, where a triage reads them; the one distinction here is the one a
/// caller can act on.
///
/// It is not [`SyscallError`] because a driver has no business naming a
/// syscall's return. The conversion happens where the two meet:
/// `vfs::FileSystem` answers `SyscallError`, [`SyscallError::Io`] is the
/// variant that exists for [`BlockError::Device`], and
/// [`SyscallError::WouldBlock`] is the one that exists for
/// [`BlockError::BudgetExpired`] — a word the ABI already had, which
/// `rust/library/std/src/sys/fs/toyos.rs` already maps to
/// `io::ErrorKind::WouldBlock`.
///
/// [`SyscallError`]: toyos_abi::syscall::SyscallError
/// [`SyscallError::Io`]: toyos_abi::syscall::SyscallError::Io
/// [`SyscallError::WouldBlock`]: toyos_abi::syscall::SyscallError::WouldBlock
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockError {
    /// The device did not do it. A transfer issued and not completed, a status
    /// the command carried, a controller recovery gave up on, a request past
    /// the end of the medium, a volume slot that is not mounted — every fact
    /// about the hardware or the volume.
    Device,
    /// The operation's [`OPERATION`] budget expired, so this was refused
    /// **before it was attempted**.
    ///
    /// Not a fact about the device: nothing was in flight when the refusal was
    /// taken, no disk was marked failed, and the transport is exactly as the
    /// last operation left it. A caller that can afford to ask again later
    /// loses nothing by doing so, and one that cannot is no worse off than the
    /// single bit left it.
    BudgetExpired,
}

impl BlockError {
    /// Which of two failures stands when one operation composes several.
    ///
    /// [`Device`](Self::Device) wins: an operation one of whose parts the
    /// device refused is a failed operation whatever else expired, and the
    /// honest answer for the whole is the worse of the halves. `page_cache::sync`
    /// is the caller — one write-back is many runs plus a flush.
    pub fn worse(self, other: Self) -> Self {
        match (self, other) {
            (Self::Device, _) | (_, Self::Device) => Self::Device,
            _ => Self::BudgetExpired,
        }
    }
}

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
