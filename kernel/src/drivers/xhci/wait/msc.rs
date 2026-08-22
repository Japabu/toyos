//! USB Mass Storage, Bulk-Only Transport with a transparent SCSI command set
//! (interface class 0x08, subclass 0x06, protocol 0x50).
//!
//! Everything that arrives here came off a wire, so nothing in this file may
//! panic on it: a capacity, a block size, a CSW tag and a residue are all
//! numbers a broken or hostile device chooses. They are checked and the device
//! is refused by name — never truncated to fit, and never trusted because the
//! transfer that carried them succeeded.

//! **This file is the tree's one user of [`crate::mm::Unaligned`]**, and the
//! reason is the Command Block Wrapper: USB BOT 1.0 §5.1 and §5.2 lay a CBW and
//! a CSW out as bytes at fixed offsets, and neither is a structure a Rust ABI
//! would produce. Nothing races those two reads and writes either — a CBW is
//! written before the transfer naming it is enqueued and a CSW is read after
//! `framed_phase` returned `Ok` — so the volatile discipline would be claiming
//! an ordering the transport already provides. Everything else here moves whole
//! buffers with `copy_from`/`copy_to`, which both disciplines share.
//!
//! `read_dma` and `write_dma`, the sweep's local four-caller wrappers for those
//! two copies, are `Dma::copy_to` and `Dma::copy_from` now.

use crate::mm::{Dma, Unaligned};

use crate::block::{BlockError, BlockResult};
use crate::log;
use crate::scheduler::Operation;
use crate::time::{Budget, Deadline, Duration};
use super::super::device::Endpoint;
use super::{Owed, Restart};
use super::super::{with_disk, Disk, StorageGeometry, Trb, TrbRing, XhciController, PAGE};
use super::super::{CC_SUCCESS, CC_STALL, CC_SHORT_PACKET, TRB_NORMAL, OFF_INPUT_CTX};
use super::super::USB_TIMEOUT_NS;
use super::super::{MSC_IN_RING, MSC_OUT_RING, MSC_CBW, MSC_CSW, MSC_SCRATCH, MSC_SCRATCH_LEN};
use super::super::{MSC_DATA, MSC_DATA_LEN, MSC_MAX_BLOCKS};

/// The block size the layer above this one is written in. A device that
/// addresses in anything this does not divide by is unimplemented, not
/// unsupported-but-approximated — see `bring_up`.
const HOST_BLOCK: u32 = 4096;

/// How long the driver spends coaxing a freshly configured device into
/// answering TEST UNIT READY.
///
/// A wall-clock budget rather than an attempt count, because the two bound
/// different things: a stick that answers NOT READY quickly deserves several
/// tries, and one that answers nothing at all has already spent the transfer
/// timeout and must not be given three more of them. Boot time is what is
/// being protected, and boot time is what this measures.
const READY_BUDGET: Budget = Budget::of(
    Duration::from_millis(500),
    "the device is reported as not becoming ready and the boot goes on without it",
);

/// How many times one SCSI command is issued when the transport breaks under
/// it.
///
/// **Derived rather than tuned.** A transfer the driver stopped waiting for is
/// still the device's to answer, and that answer can undo one Reset Recovery
/// (see [`XhciController::reset_recovery`]) — once, because there is one such
/// transfer and it is answered once. So the first attempt can break on the
/// fault itself, the second on the recovery that answer undid, and a third
/// attempt runs over a transport nothing left over is still able to disturb. A
/// command that breaks all three times is a device that is not recovering, and
/// the caller is told so rather than being made to wait for a fourth
/// [`USB_TIMEOUT_NS`].
///
/// It costs nothing on a device that is merely gone: [`XhciController::scsi`]
/// re-issues only after a Reset Recovery that *succeeded*, and a dead device
/// fails that on the class request it does not answer.
const MAX_TRANSPORT_ATTEMPTS: u8 = 3;

const CBW_SIGNATURE: u32 = 0x4342_5355;
const CSW_SIGNATURE: u32 = 0x5342_5355;
const CBW_LEN: u32 = 31;
const CSW_LEN: u32 = 13;

/// What the configuration descriptor said about a mass-storage interface.
///
/// Both endpoints, always. A value of this type cannot describe an interface
/// with one bulk endpoint or with an address this driver may not turn into a
/// device context index, because [`Endpoint`] carries a private field and so
/// can only be built by its own constructor, in the parser — so `bind` has
/// nothing left to check. The private *field* is what buys that; a private
/// constructor beside public fields would leave `bind`'s own struct literal
/// able to name any `dci` at all.
#[derive(Clone, Copy)]
pub struct MscInterface {
    pub iface_num: u8,
    pub in_ep: Endpoint,
    pub out_ep: Endpoint,
}

/// One bound disk. `Copy` because every operation takes it out of the
/// controller's vec, works on it, and writes it back — which is what lets a
/// command borrow the controller and the device's own rings at the same time
/// without the controller holding a borrow of itself.
#[derive(Clone, Copy)]
pub struct MscDevice {
    slot_id: u8,
    iface: u8,
    in_ep: u8,
    out_ep: u8,
    in_dci: u8,
    out_dci: u8,
    /// Byte offset of this device's block in its controller's DMA pool.
    block: usize,
    /// And of the *device* block, which is a different one: it holds the output
    /// device context the controller publishes this device's endpoint states
    /// in, and those states are what decides which recovery command is legal.
    dev_block: usize,
    ep0_ring: TrbRing,
    in_ring: TrbRing,
    out_ring: TrbRing,
    tag: u32,
    logical_block_bytes: u32,
    sectors_per_block: u32,
    blocks: u64,
    /// Set when recovery itself failed. The device is not spoken to again:
    /// every further command would spend the transfer timeout to learn what
    /// this already records.
    failed: bool,
    /// Set once the device has said it does not implement SYNCHRONIZE CACHE.
    ///
    /// What it buys is that the answer is reported once rather than per flush,
    /// and that is not tidiness: on a machine whose log lives on this stick, a
    /// line per flush is pending content in the ring the next flush drains, so
    /// it is the same self-sustaining write loop reading the refusal as a
    /// failure produced.
    no_write_cache: bool,
}

impl MscDevice {
    /// Whether the driver will still speak to this device.
    ///
    /// Published rather than inferred, because the geometry survives a failure:
    /// it is what the device reported before it broke, so `blocks > 0` answers
    /// "did this disk ever come up", not "is it still there".
    #[cfg(feature = "boot-actuators")]
    pub fn online(&self) -> bool {
        !self.failed
    }

    /// Which slot this disk is on, for a caller holding an event that needs to
    /// know whether a disk is behind it.
    pub fn slot_id(&self) -> u8 {
        self.slot_id
    }

    pub fn geometry(&self) -> StorageGeometry {
        StorageGeometry {
            logical_block_bytes: self.logical_block_bytes,
            blocks: self.blocks,
        }
    }

    fn next_tag(&mut self) -> u32 {
        self.tag = self.tag.wrapping_add(1);
        self.tag
    }
}

/// How one Bulk-Only round trip ended, when it ended at all.
///
/// Two variants and not three: everything that used to be `Broken` is the error
/// half of [`XhciController::bot`]'s `Result`, and `delivered` cannot exceed the
/// transfer it describes because both of the checks that bound it run before
/// this value exists.
enum Bot {
    /// CSW status 0, with the bytes of the data phase that are really there.
    ///
    /// **Two things count that number and the smaller one is the answer.** The
    /// controller reports what it moved into the buffer; the device reports in
    /// the CSW what it did not move. Keeping only the device's account — which
    /// is what this variant used to carry — means a device that under-delivers
    /// a READ(10) and then claims a residue of zero hands the caller whatever
    /// the last transfer left in the data window, from whatever LBA that was.
    Done { delivered: u32 },
    /// CSW status 1: the device understood and refused. Sense data says why.
    Failed,
}

/// Why a Bulk-Only round trip could not be completed.
///
/// One word used to stand for all of these, and `scsi` threw the rest away. On
/// a machine with no serial port that is the whole diagnosis: a T14 booting off
/// a stick said `transport broke on SCSI 0x2a` and nothing whatever about how,
/// on the one path where *what happened* is what decides which recovery command
/// is even legal.
enum Broke {
    /// The controller reported this completion code for the named phase.
    Code { phase: &'static str, code: u32 },
    /// Nothing came back for the named phase inside the transfer budget.
    Silence { phase: &'static str },
    /// The phase completed and moved the wrong number of bytes. A command block
    /// and a status block are fixed-length structures, so a short one is not a
    /// short transfer — it is a device that did not take the whole thing.
    Short { phase: &'static str, moved: u32, wanted: u32 },
    /// The endpoint stalled and the reset did not take.
    Stall { phase: &'static str },
    /// CSW status 2. The class calls this a phase error and requires Reset
    /// Recovery for it — and it is one of the shapes that leaves both endpoints
    /// *Running*, which is what makes an unconditional Reset Endpoint illegal.
    PhaseError,
    /// The CSW arrived and named somebody else's transfer.
    Csw { what: &'static str, got: u32, want: u32 },
    /// The CSW claims more bytes unmoved than the transfer had. Believing it
    /// would underflow the byte count every caller then uses to decide how much
    /// of the buffer is real.
    Residue { unmoved: u32, of: u32 },
}

impl core::fmt::Display for Broke {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Code { phase, code } => write!(f, "{phase} phase completion code {code}"),
            Self::Silence { phase } => write!(
                f,
                "no answer in the {phase} phase in {} ms",
                USB_TIMEOUT_NS / 1_000_000
            ),
            Self::Short { phase, moved, wanted } => {
                write!(f, "{phase} phase moved {moved} of {wanted} B")
            }
            Self::Stall { phase } => {
                write!(f, "the {phase} phase stalled and the endpoint reset did not clear it")
            }
            Self::PhaseError => f.write_str("the device reported a phase error"),
            Self::Csw { what, got, want } => write!(f, "CSW {what} {got:#x}, not {want:#x}"),
            Self::Residue { unmoved, of } => {
                write!(f, "CSW claims {unmoved} B unmoved of {of}")
            }
        }
    }
}

/// Abandon one bulk transfer without waiting for it, once per boot, on the
/// first WRITE(10) the driver issues.
///
/// **A kernel feature because nothing on the host side can stage it.** QEMU's
/// `usb-storage` answers every CBW, data phase and CSW it is handed, in
/// microseconds and in order; no device, drive or machine property makes one
/// bulk transfer not complete, and `rerror`/`werror` fail the whole drive rather
/// than leaving a transfer in flight. What is replaced is not a verdict — no
/// completion code is invented and no CSW is forged. The TRB really is on the
/// ring, the endpoint really is Running, and the controller really does complete
/// the transfer afterwards; only the *wait* is skipped, which is precisely the
/// state a transfer that ran out [`USB_TIMEOUT_NS`] leaves behind. So the
/// recovery below runs against a real endpoint state, which is the whole thing
/// under test. Same reason `xhci-one-slot` and `xhci-slow-connect` exist.
#[cfg(feature = "boot-actuators")]
mod transport_break {
    use core::sync::atomic::{AtomicBool, Ordering};

    static UNSPENT: AtomicBool = AtomicBool::new(true);
    static ARMED: AtomicBool = AtomicBool::new(false);

    /// Called where the driver is about to run a WRITE(10) data phase.
    pub fn arm() {
        if !crate::actuator::usb_transport_break() {
            return;
        }
        ARMED.store(UNSPENT.swap(false, Ordering::Relaxed), Ordering::Relaxed);
    }

    /// Called after the doorbell, where the wait would otherwise begin.
    pub fn take() -> bool {
        ARMED.swap(false, Ordering::Relaxed)
    }
}

/// Make the controller's account of one READ(10) data phase and the device's
/// own account of it disagree, once, where the harness asks for it.
///
/// **A kernel feature because nothing on the host side can stage it.** QEMU's
/// `usb-storage` derives the CSW residue from the same transfer the xHC
/// completed, so the two accounts of how many bytes moved are one number there
/// and can never contradict each other — no device, drive or machine property
/// makes them, and `rerror` fails the whole command rather than under-filling
/// one buffer. On real hardware they are two: a device that ends its data
/// phase early and then reports `dCSWDataResidue` as zero is a firmware bug
/// that ships, and it is the one this driver used to believe.
///
/// **What is replaced is the transfer, not the verdict.** The completion code
/// is the controller's own, the CSW is the device's own, and the bytes put back
/// into the tail of the window are the ones the *previous* transfer left there
/// — read off that window rather than invented, so what a caller is handed on
/// the unfixed path is another LBA's data exactly as it would be on the wire.
/// Only the residue is the injection's, and it names bytes that really are not
/// this transfer's. Same reason `usb-transport-break` and `xhci-one-slot`
/// exist.
#[cfg(feature = "boot-actuators")]
pub(in crate::drivers::xhci) mod short_read {
    use core::sync::atomic::{AtomicBool, Ordering};

    use crate::mm::Dma;

    /// How many bytes at the end of the buffer the controller is made not to
    /// have moved. One 512-byte sector out of the 4096 a block read asks for:
    /// a device stopping one sector short is the shape firmware bugs take, and
    /// it leaves the rest of the block correct so that what fails the caller's
    /// comparison is unambiguously the held tail.
    pub const SHORT_BY: u32 = 512;

    static ARMED: AtomicBool = AtomicBool::new(false);

    pub fn arm() {
        if !crate::actuator::usb_short_read() {
            return;
        }
        ARMED.store(true, Ordering::Relaxed);
    }

    /// The tail of a data buffer, held out of the way of the transfer about to
    /// run over it.
    pub struct Held {
        at: usize,
        bytes: [u8; SHORT_BY as usize],
    }

    /// Copy the last [`SHORT_BY`] bytes of the buffer at `at` out, if this is
    /// the transfer that was asked for.
    pub fn hold(dma: Dma<'static>, at: usize, len: u32, eligible: bool) -> Option<Held> {
        if !eligible || len < SHORT_BY || !ARMED.swap(false, Ordering::Relaxed) {
            return None;
        }
        let at = at + (len - SHORT_BY) as usize;
        let mut bytes = [0u8; SHORT_BY as usize];
        dma.copy_to(at, &mut bytes);
        Some(Held { at, bytes })
    }

    /// Put it back, and add the bytes it covers to the controller's residue.
    pub fn release(
        dma: Dma<'static>,
        held: Option<Held>,
        completion: Option<(u32, u32)>,
    ) -> Option<(u32, u32)> {
        let Some(held) = held else { return completion };
        let (code, residue) = completion?;
        dma.copy_from(held.at, &held.bytes);
        Some((code, residue + SHORT_BY))
    }
}

/// The completion of one SCSI command, after the transport's own recovery.
enum Scsi {
    Ok { delivered: u32 },
    /// The device understood the command and declined it, carrying the sense
    /// key, ASC and ASCQ it gave for declining. Carried rather than logged and
    /// dropped, because a caller issuing an *optional* command has to tell
    /// "I will not" from "I cannot" and these three bytes are the only place
    /// that answer exists.
    Refused { key: u8, asc: u8, ascq: u8 },
    /// The transport broke, or the device contradicted itself. Nothing about
    /// the buffer is known.
    Broken,
    /// The command was **not issued**: the caller's [`crate::block::OPERATION`]
    /// budget had already expired when this attempt came up.
    ///
    /// **Apart from [`Self::Broken`], because it is not a fact about the
    /// disk.** The two were one value until 2026-08-22 and
    /// `issues/boot-media/fsync-on-log-returns-other-under-a-loaded-host.md`
    /// measured what that cost: a stick that answered every transfer, a
    /// recovery that succeeded in 1 ms, and a log volume given up permanently
    /// because "your budget expired" arrived at `/bin/logd` as "this disk
    /// cannot flush". Nothing was on a ring when this is returned, no endpoint
    /// owes a completion, [`MscDevice::failed`] is clear, and the next
    /// operation finds the transport exactly as this one left it — which is
    /// what makes asking again the honest answer.
    Budget,
}

impl Scsi {
    /// SBC's ILLEGAL REQUEST / INVALID COMMAND OPERATION CODE: the device does
    /// not have this opcode. For a command SBC makes optional that is an
    /// answer and not a failure.
    fn unimplemented(&self) -> bool {
        matches!(self, Self::Refused { key: 0x05, asc: 0x20, ascq: 0x00 })
    }

    /// What this command's outcome means to [`crate::block::BlockDevice`]'s
    /// caller. `Ok` never reaches here — the three callers each have their own
    /// idea of what a complete transfer is.
    fn as_block_error(&self) -> BlockError {
        match self {
            Self::Budget => BlockError::BudgetExpired,
            _ => BlockError::Device,
        }
    }
}

/// The one line a device's refusal produces, wherever it is noticed.
///
/// One function and not three, because the three callers of [`scsi`] make the
/// same report about the same device, and a per-caller wording would make the
/// log say which code path noticed rather than what the device said.
///
/// [`scsi`]: XhciController::scsi
fn log_refusal(cdb: &[u8], key: u8, asc: u8, ascq: u8) {
    log!(
        "usb-storage: SCSI {:#04x} failed, sense {key:#04x}/{asc:#04x}/{ascq:#04x}",
        cdb.first().copied().unwrap_or(0)
    );
}

/// The sense a test makes SYNCHRONIZE CACHE answer with, in place of the
/// device's own answer, or `None` on a shipped kernel.
///
/// A kernel feature because nothing on the host side can stage it: QEMU's
/// `scsi-disk` implements 0x35 for every front end that reaches it —
/// `usb-storage` and `usb-bot` over `scsi-hd` and `scsi-block` alike — and no
/// device or drive property turns it off, while `scsi-generic` would need a
/// real host SCSI device the harness cannot assume exists. The command is
/// issued either way, so the transport under the injection is the shipped
/// transport; only the CSW's verdict is replaced. Same reason `xhci-one-slot`
/// and `i8042-fault` exist.
///
/// The two values are the two halves of the same question. ILLEGAL REQUEST /
/// INVALID COMMAND OPERATION CODE is what a conformant stick without a write
/// cache answers, and must not be a failure; HARDWARE ERROR / INTERNAL TARGET
/// FAILURE is a flush that was tried and did not work, and must reach the
/// caller as one.
fn flush_sense() -> Option<(u8, u8, u8)> {
    if crate::actuator::usb_flush_unimplemented() {
        Some((0x05, 0x20, 0x00))
    } else if crate::actuator::usb_flush_fails() {
        Some((0x04, 0x44, 0x00))
    } else {
        None
    }
}

/// Where this device's `block` field stands and what it held, taken at the top
/// of a [`XhciController::bot`] round trip.
///
/// **Because `dev.block` is a stack slot and it changed inside one call.** Two
/// of the nine deaths of one storm arm are byte-identical —
/// `KernelSlice OOB: offset=0xffff80007cae3310 size=0xd total=0x200000` out of
/// `bot`'s status phase (the wording is the bound this driver had before
/// `mm::Dma`; the same refusal now reads `DMA: 13 byte(s) at … run past a region
/// of …`) — and `0xd` is `CSW_LEN`, `0x200000` is the DMA pool, so the offset is
/// `dev.block + MSC_CSW` and `block` was holding a **kernel text address**. The
/// command phase twenty lines earlier had already narrowed the same field
/// successfully, so the write landed during the wait.
///
/// [`XhciController::with_storage`] copies the whole `Disk` out of
/// `self.msc[at]` onto its own frame and writes it back afterwards, so the
/// `&mut MscDevice` every phase holds points into **this task's kernel stack**,
/// at a fixed depth, above the frames that are running. A mid-function text
/// address landing there is a *return address*: something executed with that
/// slot as its stack pointer. The DMA bounds check is what noticed,
/// and it notices far too late to say where — this says where, the moment the
/// phase that waited comes back.
#[cfg(feature = "stack-witness")]
#[derive(Clone, Copy)]
struct BlockWitness {
    at: u64,
    was: usize,
}

#[cfg(feature = "stack-witness")]
fn block_witness(dev: &MscDevice) -> BlockWitness {
    BlockWitness { at: &raw const dev.block as u64, was: dev.block }
}

/// See [`BlockWitness`]. Nothing in this driver writes `block` after
/// [`bind_msc`] hands the device over, so any difference here is a write from
/// outside the driver entirely.
#[cfg(feature = "stack-witness")]
fn block_witness_holds(dev: &MscDevice, entered: BlockWitness) {
    let at = &raw const dev.block as u64;
    if at == entered.at && dev.block == entered.was {
        return;
    }
    // SAFETY: driver code runs on the CPU whose GS base is its own `PerCpu`.
    let (top, _) = unsafe { crate::arch::percpu::entry_stacks() };
    panic!(
        "USB BOT WITNESS: MscDevice::block changed inside one round trip — the field at \
         {at:#018x} held {:#018x} and now holds {:#018x} (the frame moved by {}). This CPU's \
         Ring 3 entry stack is {top:#018x}, so the field stands {} bytes below it and the \
         running rsp is {:#018x}. `with_storage` copies the device onto this stack, so a \
         kernel text value here is a return address something else pushed.",
        entered.was,
        dev.block,
        at.wrapping_sub(entered.at) as i64,
        top.wrapping_sub(at) as i64,
        crate::arch::cpu::read_rsp(),
    );
}

/// Which way a block transfer moves, so one batching loop serves both without
/// a `&[u8]` pretending to be a `&mut [u8]`.
enum Host<'a> {
    Into(&'a mut [u8]),
    From(&'a [u8]),
}

impl Host<'_> {
    fn len(&self) -> usize {
        match self {
            Self::Into(b) => b.len(),
            Self::From(b) => b.len(),
        }
    }
}

impl XhciController {
    /// Run `f` against the disk in this controller's `at`-th pool block,
    /// writing the device's state back whatever `f` did with it. `None` for a
    /// block with no disk behind it, which after an unplug includes one a disk
    /// used to be behind.
    fn with_storage<R>(
        &mut self,
        at: usize,
        f: impl FnOnce(&mut Self, &mut Disk) -> R,
    ) -> Option<R> {
        let mut disk = self.msc.get(at)?.disk?;
        let out = f(self, &mut disk);
        self.msc[at].disk = Some(disk);
        Some(out)
    }

    /// The three below are this driver's **operation entry points**, and the
    /// one place in it that recovers the caller's budget.
    ///
    /// Owner ruling 1B: the deadline is established by
    /// [`crate::block::begin_operation`] above `BlockDevice` and read off the
    /// running context here, because the two frames in between —
    /// `toyos_fat32::BlockAccess::read_at` and `BlockDevice::read_blocks` —
    /// cannot carry it. From here down it is an ordinary argument again, which
    /// is what keeps [`Self::scsi`] usable by `bring_up`: an enumeration is not
    /// a block-device operation, has no establishment above it, and passes
    /// [`Deadline::never`] by name.
    ///
    /// **They answer [`BlockResult`] and not `bool`**, because the budget they
    /// recover is also the budget they can *refuse* on, and that refusal is not
    /// a fact about the disk. One word for both is what
    /// `issues/boot-media/fsync-on-log-returns-other-under-a-loaded-host.md`
    /// measured the cost of; [`Scsi::Budget`] carries the difference up.
    pub(super) fn msc_read(&mut self, at: usize, lba: u64, count: u32, buf: &mut [u8]) -> BlockResult {
        let until = Operation::deadline();
        self.with_storage(at, |ctrl, disk| {
            ctrl.transfer_blocks(&mut disk.dev, lba, count, Host::Into(buf), until)
        })
        // No disk under this index: the controller has nothing to ask, which is
        // a device fact and never a budget.
        .unwrap_or(Err(BlockError::Device))
    }

    pub(super) fn msc_write(&mut self, at: usize, lba: u64, count: u32, buf: &[u8]) -> BlockResult {
        let until = Operation::deadline();
        self.with_storage(at, |ctrl, disk| {
            ctrl.transfer_blocks(&mut disk.dev, lba, count, Host::From(buf), until)
        })
        .unwrap_or(Err(BlockError::Device))
    }

    pub(super) fn msc_flush(&mut self, at: usize) -> BlockResult {
        let until = Operation::deadline();
        self.with_storage(at, |ctrl, disk| {
            let number = disk.index;
            let dev = &mut disk.dev;
            if dev.failed {
                return Err(BlockError::Device);
            }
            // LBA 0, block count 0: the whole medium, which is the only thing
            // a cache flush above a block device can mean.
            let cdb = [0x35u8, 0, 0, 0, 0, 0, 0, 0, 0, 0];
            let issued = ctrl.scsi(dev, &cdb, 10, 0, 0, false, until);
            let outcome = match flush_sense() {
                Some((key, asc, ascq)) => Scsi::Refused { key, asc, ascq },
                None => issued,
            };
            // SYNCHRONIZE CACHE is optional in SBC and a great many USB sticks
            // do not have it. A device with no write cache has nothing this
            // command could have made durable, so the writes before it are
            // already as durable as they will get: reporting a failure reports
            // the wrong thing, and the caller above turns a failed sync into a
            // log line, which is itself the next flush.
            if outcome.unimplemented() {
                if !dev.no_write_cache {
                    dev.no_write_cache = true;
                    log!("usb-storage: disk {number} does not implement SYNCHRONIZE CACHE \
                         (sense 0x05/0x20/0x00); its writes are durable once they complete");
                }
                return Ok(());
            }
            match outcome {
                Scsi::Ok { .. } => Ok(()),
                Scsi::Refused { key, asc, ascq } => {
                    log_refusal(&cdb, key, asc, ascq);
                    Err(BlockError::Device)
                }
                Scsi::Broken => Err(BlockError::Device),
                // The flush was never issued. Unlogged here — `scsi` already
                // named the budget in the line it wrote — and unlogged above,
                // because `FatFs::sync`'s own doc says a line written on the
                // log mount is the next flush.
                Scsi::Budget => Err(BlockError::BudgetExpired),
            }
        })
        .unwrap_or(Err(BlockError::Device))
    }

    /// Move `count` 4 KiB blocks between the caller's buffer and the disk.
    ///
    /// `until` bounds the whole of it and not one command: the loop below is
    /// `ceil(count / MSC_MAX_BLOCKS)` commands, and it is [`Self::scsi`] that
    /// refuses to start one past the deadline.
    fn transfer_blocks(
        &mut self,
        dev: &mut MscDevice,
        lba: u64,
        count: u32,
        mut host: Host<'_>,
        until: Deadline,
    ) -> BlockResult {
        let write = matches!(host, Host::From(_));
        // The caller is the kernel and the trait states this contract, so a
        // mismatch is a kernel bug and gets fail-fast. Everything below this
        // line is about the *device's* numbers, which get refusals instead.
        assert_eq!(host.len(), count as usize * HOST_BLOCK as usize);
        if dev.failed {
            return Err(BlockError::Device);
        }
        if count == 0 {
            return Ok(());
        }
        match lba.checked_add(count as u64) {
            Some(end) if end <= dev.blocks => {}
            _ => {
                log!("usb-storage: {lba}+{count} is past the {} blocks this disk has", dev.blocks);
                return Err(BlockError::Device);
            }
        }

        let dma = self.dma();
        let data_phys = dma.phys() + (dev.block + MSC_DATA) as u64;
        let mut done = 0u32;
        while done < count {
            let batch = (count - done).min(MSC_MAX_BLOCKS);
            let bytes = batch as usize * HOST_BLOCK as usize;
            let offset = done as usize * HOST_BLOCK as usize;
            let sector_lba = (lba + done as u64) * dev.sectors_per_block as u64;
            let sectors = batch * dev.sectors_per_block;

            // `bring_up` refused any disk whose last sector does not fit a
            // 32-bit LBA, so this driver's READ(10)/WRITE(10) can address
            // every block it reported.
            let lba32 = sector_lba as u32;
            let cdb = [
                if write { 0x2Au8 } else { 0x28 },
                0,
                (lba32 >> 24) as u8,
                (lba32 >> 16) as u8,
                (lba32 >> 8) as u8,
                lba32 as u8,
                0,
                (sectors >> 8) as u8,
                sectors as u8,
                0,
            ];

            if let Host::From(src) = &host {
                dma.copy_from(dev.block + MSC_DATA, &src[offset..offset + bytes]);
            }

            match self.scsi(dev, &cdb, 10, data_phys, bytes as u32, !write, until) {
                Scsi::Ok { delivered } if delivered as usize == bytes => {}
                // Short of what was asked, and reported as success. Nothing
                // above here has a way to say "these blocks arrived and those
                // did not", so a partial transfer is a failed one.
                Scsi::Ok { delivered } => {
                    log!("usb-storage: {delivered} of {bytes} B at block {}", lba + done as u64);
                    return Err(BlockError::Device);
                }
                Scsi::Refused { key, asc, ascq } => {
                    log_refusal(&cdb, key, asc, ascq);
                    return Err(BlockError::Device);
                }
                // **A partial transfer whose remainder ran out of budget is a
                // failure of the whole operation and not a retryable one.** The
                // blocks already moved are on the device and the caller's buffer
                // half describes them, so `done > 0` is a state no re-issue can
                // resume from — only the first batch may honestly answer "ask
                // again". Every caller in this kernel transfers eight blocks or
                // fewer (`MSC_MAX_BLOCKS`), so the loop turns once and this is
                // the reachable arm; it is written for the day one does not.
                other @ (Scsi::Broken | Scsi::Budget) => {
                    return Err(if done == 0 { other.as_block_error() } else { BlockError::Device });
                }
            }

            if let Host::Into(dst) = &mut host {
                dma.copy_to(dev.block + MSC_DATA, &mut dst[offset..offset + bytes]);
            }
            done += batch;
        }
        Ok(())
    }

    /// One SCSI command, with the transport's recovery applied and the command
    /// re-issued over the transport that recovery gave back. `Scsi::Ok` means
    /// the device reported success and moved `delivered` bytes.
    ///
    /// **Reset Recovery restores the transport and says nothing about the
    /// command**, so a driver that recovers and then reports failure has thrown
    /// away a write it could have completed — the T14's boot disk losing a
    /// block to one transport hiccup. Every CDB this file issues is
    /// idempotent, and the caller's bytes are still in the same DMA window
    /// nothing between two attempts touches, so an attempt is a genuine
    /// re-issue of the same command and not an approximation of one.
    ///
    /// # The caller's budget, and why it is spent *here*
    ///
    /// `until` is [`crate::block::OPERATION`]'s deadline for the whole
    /// operation this command is part of, and this is the one place in the
    /// driver that reads it. Not because it is convenient: it is the only place
    /// where a refusal costs the device nothing. Between two commands there is
    /// no TRB on a ring, no endpoint owing a completion and no phase half done,
    /// so refusing here is a decision about the *caller's* time and never a
    /// verdict about the disk — nothing is abandoned, [`MscDevice::failed`]
    /// stays clear, and the next operation finds the transport exactly as this
    /// one left it. Taking the same decision one level down, inside
    /// [`Self::bot`] or its recovery, would abandon a transfer the device is
    /// still going to answer and then read the wreckage as a device that is not
    /// recovering — a slow disk marked permanently offline for having been slow.
    ///
    /// So what this bounds is a device that *answers*, which is exactly the
    /// failure `USB_TIMEOUT_NS` cannot see: that bound is only ever reached by a
    /// device that has stopped answering, and a stick that completes every
    /// transfer in 2 ms can still hold one `read_blocks` for as long as the
    /// batching, the retries and the recoveries take. The overshoot is the
    /// command in flight when the deadline passes, which the transfer bound
    /// covers.
    #[allow(clippy::too_many_arguments)]
    fn scsi(
        &mut self,
        dev: &mut MscDevice,
        cdb: &[u8],
        cdb_len: u8,
        data_phys: u64,
        data_len: u32,
        data_in: bool,
        until: Deadline,
    ) -> Scsi {
        let opcode = cdb.first().copied().unwrap_or(0);
        // Which disk this command is on, in every line the retry writes. A
        // machine that boots off USB carries at least two — the stick it
        // started from and whatever else is plugged in — and an unnamed
        // `transport broke` cannot be attributed to either, which is how a
        // harness assertion came to count the boot stick's own recovery against
        // the disk under test (`issues/hardware/`).
        let slot = self.slot(dev.slot_id);
        for attempt in 1..=MAX_TRANSPORT_ATTEMPTS {
            if until.reached(crate::clock::now()) {
                log!("usb-storage: {slot} SCSI {opcode:#04x} not issued: {}",
                    crate::block::OPERATION);
                // Not `Broken`: nothing was issued, nothing is on a ring, and
                // `dev.failed` is untouched. The two were one value until
                // 2026-08-22, which is what made `/bin/logd` give up a volume
                // on a stick that was answering.
                return Scsi::Budget;
            }
            match self.bot(dev, cdb, cdb_len, data_phys, data_len, data_in) {
                Ok(Bot::Done { delivered }) => {
                    if attempt > 1 {
                        log!("usb-storage: {slot} SCSI {opcode:#04x} completed on attempt \
                             {attempt}");
                    }
                    return Scsi::Ok { delivered };
                }
                Ok(Bot::Failed) => {
                    let (key, asc, ascq) = self.request_sense(dev);
                    return Scsi::Refused { key, asc, ascq };
                }
                Err(broke) => {
                    log!("usb-storage: {slot} transport broke on SCSI {opcode:#04x}: {broke}");
                    if !self.reset_recovery(dev) {
                        log!("usb-storage: {slot} reset recovery failed; disk is offline");
                        dev.failed = true;
                        return Scsi::Broken;
                    }
                }
            }
        }
        log!("usb-storage: {slot} SCSI {opcode:#04x} broke {MAX_TRANSPORT_ATTEMPTS} times \
             running; the transport is not coming back on its own");
        Scsi::Broken
    }

    /// REQUEST SENSE, as (sense key, ASC, ASCQ) and zeroed if the device would
    /// not say. Never a decision about the *transport*, which is what
    /// [`Bot::Broken`] covers and what recovery answers; the sense bytes decide
    /// only what the device meant by declining a command it understood, and
    /// zeroes fall on the failing side of every such decision.
    fn request_sense(&mut self, dev: &mut MscDevice) -> (u8, u8, u8) {
        let dma = self.dma();
        let phys = dma.phys() + (dev.block + MSC_SCRATCH) as u64;
        super::super::zero_dma(dma, dev.block + MSC_SCRATCH, MSC_SCRATCH_LEN);
        let cdb = [0x03u8, 0, 0, 0, 18, 0];
        // Recursion is not possible: a failing REQUEST SENSE goes through
        // `bot` directly, so it cannot ask for sense data about itself.
        // ASCQ is byte 13 of the fixed-format sense data, so fourteen bytes is
        // what makes all three readable. Short of that they are whatever the
        // zeroing above left, and zero ASC/ASCQ is exactly the value
        // [`Scsi::unimplemented`] tests for.
        match self.bot(dev, &cdb, 6, phys, 18, true) {
            Ok(Bot::Done { delivered }) if delivered >= 14 => {
                let mut resp = [0u8; 18];
                dma.copy_to(dev.block + MSC_SCRATCH, &mut resp);
                (resp[2] & 0x0F, resp[12], resp[13])
            }
            _ => (0, 0, 0),
        }
    }

    /// One fixed-length leg of the round trip — the command block or the status
    /// block — which the device has to take or give in full. `what` names it in
    /// the line a break produces.
    fn framed_phase(
        &mut self,
        dev: &mut MscDevice,
        in_dir: bool,
        phys: u64,
        len: u32,
        what: &'static str,
    ) -> Result<(), Broke> {
        match self.bulk(dev, in_dir, phys, len) {
            // Short Packet is how an xHC reports a transfer that ended on a
            // packet smaller than the endpoint's maximum, which a 13-byte CSW
            // on a 512-byte endpoint is. With no residue behind it the whole
            // block arrived, and reading the code alone made a complete status
            // phase an error.
            Some((CC_SUCCESS | CC_SHORT_PACKET, 0)) => Ok(()),
            Some((CC_SUCCESS | CC_SHORT_PACKET, residue)) => Err(Broke::Short {
                phase: what,
                moved: len.saturating_sub(residue),
                wanted: len,
            }),
            Some((code, _)) => Err(Broke::Code { phase: what, code }),
            None => Err(Broke::Silence { phase: what }),
        }
    }

    /// The Bulk-Only Transport round trip: command block out, data, status in.
    fn bot(
        &mut self,
        dev: &mut MscDevice,
        cdb: &[u8],
        cdb_len: u8,
        data_phys: u64,
        data_len: u32,
        data_in: bool,
    ) -> Result<Bot, Broke> {
        // The CDBs are this file's own, so their shape is a kernel invariant.
        assert!(cdb_len as usize <= cdb.len() && cdb_len <= 16);
        assert!(data_len as usize <= MSC_DATA_LEN);

        let dma = self.dma();
        let tag = dev.next_tag();
        #[cfg(feature = "stack-witness")]
        let entered_with = block_witness(dev);
        // The unaligned discipline, and the file header says why. Bounded by
        // each write against `CBW_LEN`: the last field ends at `15 + cdb_len`,
        // `cdb_len <= 16` was asserted on entry, and `CBW_LEN` is 31. Exclusive:
        // the transfer naming this block has not been enqueued yet, and a device
        // block belongs to one device.
        let cbw: Dma<'static, Unaligned> =
            super::super::zero_dma(dma, dev.block + MSC_CBW, CBW_LEN as usize).unaligned();
        cbw.write::<u32>(0, CBW_SIGNATURE.to_le());
        cbw.write::<u32>(4, tag.to_le());
        cbw.write::<u32>(8, data_len.to_le());
        cbw.write::<u8>(12, if data_in { 0x80 } else { 0x00 });
        cbw.write::<u8>(13, 0); // LUN 0: this driver binds one logical unit
        cbw.write::<u8>(14, cdb_len);
        cbw.copy_from(15, &cdb[..cdb_len as usize]);

        let cbw_phys = dma.phys() + (dev.block + MSC_CBW) as u64;
        self.framed_phase(dev, false, cbw_phys, CBW_LEN, "command")?;

        // What the *controller* says reached the buffer, which is the account
        // the CSW's residue is checked against below.
        let mut moved = 0u32;
        if data_len > 0 {
            #[cfg(feature = "boot-actuators")]
            if cdb.first() == Some(&0x2A) {
                transport_break::arm();
            }
            #[cfg(feature = "boot-actuators")]
            let held = short_read::hold(
                dma,
                (data_phys - dma.phys()) as usize,
                data_len,
                data_in && cdb.first() == Some(&0x28),
            );
            let completion = self.bulk(dev, data_in, data_phys, data_len);
            #[cfg(feature = "boot-actuators")]
            let completion = short_read::release(dma, held, completion);
            match completion {
                Some((CC_SUCCESS | CC_SHORT_PACKET, unmoved)) => {
                    moved = data_len.saturating_sub(unmoved);
                }
                // A stalled data phase is ordinary — an unsupported command
                // or a read past the end stalls here — and the CSW still
                // arrives once the endpoint is unhalted. Recovering and then
                // reading the status is what turns it into a clean refusal.
                Some((CC_STALL, unmoved)) => {
                    if !self.restart_bulk(dev, data_in) {
                        return Err(Broke::Stall { phase: "data" });
                    }
                    moved = data_len.saturating_sub(unmoved);
                }
                Some((code, _)) => return Err(Broke::Code { phase: "data", code }),
                None => return Err(Broke::Silence { phase: "data" }),
            }
        }

        let csw_phys = dma.phys() + (dev.block + MSC_CSW) as u64;
        super::super::zero_dma(dma, dev.block + MSC_CSW, CSW_LEN as usize);
        let mut got = self.framed_phase(dev, true, csw_phys, CSW_LEN, "status");
        if let Err(Broke::Code { code: CC_STALL, .. }) = got {
            // The spec's one legal retry: the device may stall the status
            // phase once, and a second stall means it has lost the plot.
            if !self.restart_bulk(dev, true) {
                return Err(Broke::Stall { phase: "status" });
            }
            super::super::zero_dma(dma, dev.block + MSC_CSW, CSW_LEN as usize);
            got = self.framed_phase(dev, true, csw_phys, CSW_LEN, "status");
        }
        got?;

        #[cfg(feature = "stack-witness")]
        block_witness_holds(dev, entered_with);
        // The unaligned discipline again, for the CSW's half of the header's
        // argument. Bounded by each read against the `CSW_LEN`-byte subview; the
        // last field is a `u8` at 12 of 13. The transfer has completed —
        // `framed_phase` returned `Ok` above — so the device is not writing this
        // block. Every number read here is the device's and is checked on the
        // lines below, never believed.
        let csw = dma.subview(dev.block + MSC_CSW, CSW_LEN as usize).unaligned();
        let (signature, csw_tag, residue, status) = (
            u32::from_le(csw.read::<u32>(0)),
            u32::from_le(csw.read::<u32>(4)),
            u32::from_le(csw.read::<u32>(8)),
            csw.read::<u8>(12),
        );
        if signature != CSW_SIGNATURE {
            return Err(Broke::Csw { what: "signature", got: signature, want: CSW_SIGNATURE });
        }
        // A CSW carrying somebody else's tag is a device out of step with the
        // driver, and accepting it would attribute one command's status to
        // another — the failure mode where a write reports the success of the
        // read before it.
        if csw_tag != tag {
            return Err(Broke::Csw { what: "tag", got: csw_tag, want: tag });
        }
        if residue > data_len {
            return Err(Broke::Residue { unmoved: residue, of: data_len });
        }
        match status {
            // The device's account of the transfer and the controller's, and
            // the driver keeps neither on trust: what a caller may read is what
            // both of them say arrived.
            0 => Ok(Bot::Done { delivered: moved.min(data_len - residue) }),
            1 => Ok(Bot::Failed),
            _ => Err(Broke::PhaseError),
        }
    }

    /// One Normal TRB on a bulk endpoint, and its completion.
    fn bulk(
        &mut self,
        dev: &mut MscDevice,
        in_dir: bool,
        phys: u64,
        len: u32,
    ) -> Option<(u32, u32)> {
        let (dci, ring) = if in_dir {
            (dev.in_dci, &mut dev.in_ring)
        } else {
            (dev.out_dci, &mut dev.out_ring)
        };
        let mut trb = Trb::ZERO;
        trb.param = phys;
        trb.status = len;
        // ISP so a device that sends less than asked reports it instead of
        // leaving the transfer outstanding, IOC so it reports at all.
        trb.control = TRB_NORMAL | (1 << 5) | (1 << 2);
        let at = ring.enqueue(trb);
        let slot = dev.slot_id;
        self.ring_doorbell(slot, dci);
        #[cfg(feature = "boot-actuators")]
        if transport_break::take() {
            return None;
        }
        self.wait_transfer(slot, dci, at)
    }

    /// One of this disk's bulk endpoints, as the recovery needs to see it.
    ///
    /// The route is [`XhciController::restart_endpoint`]'s to choose, because
    /// which command is legal is a property of the endpoint's state and nothing
    /// about that is per class. All this decides is which of the pair is meant.
    fn bulk_endpoint<'a>(dev: &'a mut MscDevice, in_dir: bool) -> Restart<'a> {
        let (dci, ep_addr, ring_off) = if in_dir {
            (dev.in_dci, dev.in_ep, MSC_IN_RING)
        } else {
            (dev.out_dci, dev.out_ep, MSC_OUT_RING)
        };
        Restart {
            slot_id: dev.slot_id,
            ctx_block: dev.dev_block,
            dci,
            ep_addr,
            ring_at: dev.block + ring_off,
            ring: if in_dir { &mut dev.in_ring } else { &mut dev.out_ring },
            ep0_ring: &mut dev.ep0_ring,
        }
    }

    /// One of this disk's bulk endpoints, back to a state that runs TRBs.
    fn restart_bulk(&mut self, dev: &mut MscDevice, in_dir: bool) -> bool {
        self.restart_endpoint(Self::bulk_endpoint(dev, in_dir))
    }

    /// Bulk-Only Mass Storage Reset plus whatever each endpoint needs: what the
    /// class specification requires once the device's command/data/status state
    /// machine no longer agrees with the driver's.
    ///
    /// **Both endpoints come off their transfers before the device is spoken
    /// to, and that order is the whole of the correctness.** The transfer this
    /// recovers from is one the driver stopped waiting for, which is not one
    /// that is over: it is still the controller's to run and still the device's
    /// to answer. A class request issued into that window is undone by the
    /// answer when it lands — the device finishes the command it was given, is
    /// left owing a status block again, and stalls the next command block the
    /// driver sends. That is a second broken transfer out of one fault, and a
    /// caller's write lost with it. Whether the window opens at all is a race
    /// between the guest and the device, which is why the dev host has never
    /// seen it and CI reproduces it every run (`issues/hardware/`).
    ///
    /// [`Owed`](super::Owed) is what keeps that order rather than a comment.
    /// Everything this says to the device is [`Self::reset_the_device`], whose
    /// arguments are the two endpoints' quiesces — so there is no order of
    /// these three lines that speaks first.
    fn reset_recovery(&mut self, dev: &mut MscDevice) -> bool {
        let owed_in = self.quiesce_endpoint(&mut Self::bulk_endpoint(dev, true));
        let owed_out = self.quiesce_endpoint(&mut Self::bulk_endpoint(dev, false));
        self.reset_the_device(dev, owed_in, owed_out)
    }

    /// Every word of Reset Recovery that reaches the device, in BOT §5.3.4's
    /// order: the class request, then a CLEAR_FEATURE for each endpoint that is
    /// halted.
    ///
    /// It takes both endpoints' [`Owed`](super::Owed) because the class request
    /// may not go out until the controller has taken both of them off their
    /// transfers — see [`Self::reset_recovery`]. Reading them afterwards would
    /// have been the same code with the guarantee taken out.
    fn reset_the_device(&mut self, dev: &mut MscDevice, in_ep: Owed, out_ep: Owed) -> bool {
        let slot = dev.slot_id;
        let iface = dev.iface as u16;
        let reset = self.control_transfer(slot, &mut dev.ep0_ring, 0x21, 0xFF, 0, iface, None, 0);
        if !reset.done() {
            log!("usb-storage: slot {slot} would not take a Bulk-Only Reset: {reset}");
        }
        // Both halts are cleared even if the class request did not land: the
        // endpoints are what the next command touches, and leaving one halted
        // because another step failed turns a recoverable device into a
        // permanently offline one.
        let cleared_in = self.clear_endpoint_halt(slot, &mut dev.ep0_ring, in_ep);
        let cleared_out = self.clear_endpoint_halt(slot, &mut dev.ep0_ring, out_ep);
        reset.done() && cleared_in && cleared_out
    }
}

/// One mass-storage device's pool block and the two bulk rings its endpoint
/// contexts name, as [`prepare`] left them.
///
/// Carried from the Configure Endpoint act to the bind behind it rather than
/// rebuilt there: `TrbRing::init` zeroes, and by then the memory is the
/// controller's to read.
#[derive(Clone, Copy)]
pub(in crate::drivers::xhci) struct MscRings {
    /// Which of [`super::super::MSC_BLOCKS`] this is, which is what the teardown gives
    /// back.
    at: usize,
    block: usize,
    in_ring: TrbRing,
    out_ring: TrbRing,
}

/// Claim a pool block for this device and write its two bulk endpoints into the
/// input context, ready for the Configure Endpoint the sequence issues.
///
/// **Claimed before the endpoints are configured** and released only by the
/// teardown that disables this slot, so the block cannot be reissued while the
/// previous holder's contexts still name it. `storage.len()` was the old answer
/// and is a different question: it counts the disks that *finished*.
///
/// `None` when the pool is out, which is a refusal and not a failure — nothing
/// is spent, because nothing was handed out.
pub(in crate::drivers::xhci) fn prepare(
    ctrl: &mut XhciController,
    slot_id: u8,
    speed: u8,
    port_idx: u8,
    info: &MscInterface,
) -> Option<MscRings> {
    let Some((at, block)) = ctrl.claim_msc_block(port_idx) else {
        log!("usb-storage: slot {slot_id} is the {}th disk; this driver serves {}",
            ctrl.msc_blocks_taken() + 1, super::super::MSC_BLOCKS);
        return None;
    };

    let (in_dci, out_dci) = (info.in_ep.dci(), info.out_ep.dci());
    let dma = ctrl.dma();
    let in_ring = TrbRing::init(dma.subview(block + MSC_IN_RING, PAGE));
    let out_ring = TrbRing::init(dma.subview(block + MSC_OUT_RING, PAGE));

    let input_ctx = super::super::zero_dma(dma, OFF_INPUT_CTX, PAGE);
    ctrl.write_ctx32(input_ctx, 0, 1, 1 | (1u32 << in_dci) | (1u32 << out_dci));
    let max_dci = in_dci.max(out_dci) as u32;
    ctrl.write_ctx32(input_ctx, 1, 0, ((speed as u32) << 20) | (max_dci << 27));
    ctrl.write_ctx32(input_ctx, 1, 1, (port_idx as u32 + 1) << 16);

    // EP Type 2 is Bulk Out and 6 is Bulk In; CErr 3 is the retry count the
    // controller applies before reporting a transaction error. Average TRB
    // Length is advisory — the controller uses it for bandwidth bookkeeping —
    // and the endpoint's own maximum packet size is the honest answer for a
    // driver that issues one TRB per transfer.
    for (dci, ep_type, mps, burst, ring) in [
        (out_dci, 2u32, info.out_ep.max_packet, info.out_ep.max_burst, &out_ring),
        (in_dci, 6u32, info.in_ep.max_packet, info.in_ep.max_burst, &in_ring),
    ] {
        let ctx = dci as usize + 1;
        ctrl.write_ctx32(input_ctx, ctx, 0, 0);
        ctrl.write_ctx32(
            input_ctx,
            ctx,
            1,
            (3 << 1) | (ep_type << 3) | ((burst as u32) << 8) | ((mps as u32) << 16),
        );
        let dequeue = ring.dequeue();
        ctrl.write_ctx32(input_ctx, ctx, 2, dequeue as u32);
        ctrl.write_ctx32(input_ctx, ctx, 3, (dequeue >> 32) as u32);
        ctrl.write_ctx32(input_ctx, ctx, 4, mps as u32);
    }
    Some(MscRings { at, block, in_ring, out_ring })
}

/// Ask the disk what it is, and register it if the answer is one this driver
/// serves. `dev_block` is the device's own block, which is where its EP0 ring
/// already lives.
///
/// **The last blocking path a scheduler pass can reach**, and the one door in
/// the split X2b builds: everything below is Bulk-Only Transport, which is a
/// machine of its own and does not have one yet
/// (`issues/hardware/the-bot-scsi-machine-is-still-hand-written-in-the-kernel.md`).
/// A hot-plugged disk has to be brought up by *some*body and there is
/// no other context that may block, so until then this runs where it always
/// did.
///
/// No return value: every failure path below logs, so there was nothing in the
/// `bool` this used to produce that the one caller wanted — and it discarded it
/// in statement position, silently, because Rust does not warn about a dropped
/// `bool`.
pub(in crate::drivers::xhci) fn bind(
    ctrl: &mut XhciController,
    ep0_ring: TrbRing,
    slot_id: u8,
    dev_block: usize,
    rings: MscRings,
    info: &MscInterface,
) {
    let MscRings { at, block, in_ring, out_ring } = rings;
    let mut dev = MscDevice {
        slot_id,
        iface: info.iface_num,
        in_ep: info.in_ep.addr,
        out_ep: info.out_ep.addr,
        in_dci: info.in_ep.dci(),
        out_dci: info.out_ep.dci(),
        block,
        dev_block,
        ep0_ring,
        in_ring,
        out_ring,
        tag: 0,
        logical_block_bytes: 0,
        sectors_per_block: 0,
        blocks: 0,
        failed: false,
        no_write_cache: false,
    };

    if !bring_up(ctrl, &mut dev) {
        return;
    }
    // The machine-wide number, taken here because here is where there is a disk
    // to give one to: it is what `usb_storage::open` indexes by and what a mount
    // holds for its whole life, so it must not move when some other controller
    // binds or loses a disk — see [`super::super::DISKS_BOUND`].
    let index = super::super::DISKS_BOUND.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    log!(
        "usb-storage: disk {index} ready on slot {slot_id}, {} blocks of {} B \
         ({} MiB), msc_block +{:#x}",
        dev.blocks,
        dev.logical_block_bytes,
        dev.blocks * HOST_BLOCK as u64 / (1024 * 1024),
        block
    );
    ctrl.msc[at].disk = Some(Disk { index, dev });
}

/// TEST UNIT READY, INQUIRY and READ CAPACITY: everything between a configured
/// interface and a disk with a size.
fn bring_up(ctrl: &mut XhciController, dev: &mut MscDevice) -> bool {
    // Driving the transport rather than `scsi` here, for two reasons: a device
    // that answers NOT READY is expected rather than an error, so it must not
    // produce a log line per attempt, and the sense fetch that reports it is
    // also what clears the condition on a stick still spinning up.
    let give_up = crate::clock::nanos_since_boot() + READY_BUDGET.nanos();
    let mut sense = (0u8, 0u8, 0u8);
    let mut ready = false;
    loop {
        match ctrl.bot(dev, &[0x00u8; 6], 6, 0, 0, false) {
            Ok(Bot::Done { .. }) => {
                ready = true;
                break;
            }
            Ok(Bot::Failed) => sense = ctrl.request_sense(dev),
            Err(broke) => {
                log!("usb-storage: slot {} broke on TEST UNIT READY: {broke}", dev.slot_id);
                if !ctrl.reset_recovery(dev) {
                    dev.failed = true;
                }
            }
        }
        if dev.failed || crate::clock::nanos_since_boot() >= give_up {
            break;
        }
    }
    if !ready {
        log!("usb-storage: slot {} never became ready, sense {:#04x}/{:#04x}/{:#04x}",
            dev.slot_id, sense.0, sense.1, sense.2);
        return false;
    }

    let dma = ctrl.dma();
    let scratch_phys = dma.phys() + (dev.block + MSC_SCRATCH) as u64;
    // **No caller's budget here, and that is not an omission.**
    // [`crate::block::OPERATION`] bounds one *block-device operation*, and a
    // bring-up is not one: nobody has asked for anything yet, there is no
    // `BlockDevice` handle to hand an `Err` to, and the two bounds this
    // sequence does answer to are its own — `READY_BUDGET` above and the
    // transport's `USB_TIMEOUT_NS` under every transfer. A device that never
    // finishes bring-up is refused by name and never becomes a disk, which is
    // the give-up policy at this layer.
    let until = Deadline::never();
    let read_scratch = |ctrl: &mut XhciController,
                        dev: &mut MscDevice,
                        cdb: &[u8],
                        cdb_len: u8,
                        want: u32,
                        out: &mut [u8]| {
        super::super::zero_dma(dma, dev.block + MSC_SCRATCH, MSC_SCRATCH_LEN);
        match ctrl.scsi(dev, cdb, cdb_len, scratch_phys, want, true, until) {
            Scsi::Ok { delivered } if delivered as usize >= out.len() => {
                dma.copy_to(dev.block + MSC_SCRATCH, out);
                true
            }
            Scsi::Refused { key, asc, ascq } => {
                log_refusal(cdb, key, asc, ascq);
                false
            }
            _ => false,
        }
    };

    let mut inquiry = [0u8; 36];
    if !read_scratch(ctrl, dev, &[0x12u8, 0, 0, 0, 36, 0], 6, 36, &mut inquiry) {
        log!("usb-storage: slot {} would not answer INQUIRY", dev.slot_id);
        return false;
    }
    let peripheral = inquiry[0] & 0x1F;
    if peripheral != 0 {
        log!("usb-storage: slot {} is SCSI peripheral type {peripheral:#04x}, not a disk",
            dev.slot_id);
        return false;
    }
    log!("usb-storage: slot {} vendor {} product {}", dev.slot_id,
        Printable(&inquiry[8..16]), Printable(&inquiry[16..32]));

    // READ CAPACITY(10) reports an all-ones last LBA when the disk is too big
    // to describe in 32 bits, which is the device asking for the 16-byte form
    // rather than an answer.
    let mut cap10 = [0u8; 8];
    if !read_scratch(ctrl, dev, &[0x25u8, 0, 0, 0, 0, 0, 0, 0, 0, 0], 10, 8, &mut cap10) {
        log!("usb-storage: slot {} would not answer READ CAPACITY(10)", dev.slot_id);
        return false;
    }
    let (last_lba, block_bytes) = if u32::from_be_bytes([cap10[0], cap10[1], cap10[2], cap10[3]])
        == u32::MAX
    {
        let mut cap16 = [0u8; 12];
        let cdb = [0x9Eu8, 0x10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 32, 0, 0];
        if !read_scratch(ctrl, dev, &cdb, 16, 32, &mut cap16) {
            log!("usb-storage: slot {} would not answer READ CAPACITY(16)", dev.slot_id);
            return false;
        }
        (
            u64::from_be_bytes([
                cap16[0], cap16[1], cap16[2], cap16[3], cap16[4], cap16[5], cap16[6], cap16[7],
            ]),
            u32::from_be_bytes([cap16[8], cap16[9], cap16[10], cap16[11]]),
        )
    } else {
        (
            u32::from_be_bytes([cap10[0], cap10[1], cap10[2], cap10[3]]) as u64,
            u32::from_be_bytes([cap10[4], cap10[5], cap10[6], cap10[7]]),
        )
    };

    // Every number below came off the wire. A block size of zero divides, a
    // block size above 4096 makes `4096 / block_bytes` zero and then divides
    // by *that* — which is exactly the `#DE` an 8 KiB NVMe namespace produced
    // before the same check went into that driver. The set is not policy: it
    // is which sizes divide the 4 KiB block everything above here is written
    // in.
    if !matches!(block_bytes, 512 | 1024 | 2048 | 4096) {
        log!("usb-storage: slot {} reports {block_bytes}-byte blocks; this driver \
             serves 4096-byte blocks and needs 512..=4096", dev.slot_id);
        return false;
    }
    // READ(10) and WRITE(10) carry a 32-bit LBA, so a disk whose last sector
    // does not fit one has blocks this driver cannot address. Serving the
    // first 2 TiB of it would be a silent truncation of the device.
    if last_lba > u32::MAX as u64 {
        log!("usb-storage: slot {} has {} sectors; this driver issues READ(10) and \
             addresses 2^32", dev.slot_id, last_lba as u128 + 1);
        return false;
    }
    let sectors = last_lba + 1;
    let sectors_per_block = HOST_BLOCK / block_bytes;
    let blocks = sectors / sectors_per_block as u64;
    if blocks == 0 {
        log!("usb-storage: slot {} holds {sectors} sectors of {block_bytes} B, less \
             than one 4096-byte block", dev.slot_id);
        return false;
    }

    dev.logical_block_bytes = block_bytes;
    dev.sectors_per_block = sectors_per_block;
    dev.blocks = blocks;
    true
}

/// A device-supplied ASCII field, rendered without letting it choose what the
/// log looks like.
struct Printable<'a>(&'a [u8]);

impl core::fmt::Display for Printable<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("\"")?;
        let mut utf8 = [0u8; 4];
        for &b in self.0 {
            let c = if (0x20..0x7F).contains(&b) && b != b'"' { b as char } else { '.' };
            f.write_str(c.encode_utf8(&mut utf8))?;
        }
        f.write_str("\"")
    }
}
/// Read `count` 4 KiB blocks at `lba`. On `Err` the transfer did not happen and
/// `buf` holds nothing the caller may believe.
///
/// **The caller must be inside a block-device operation**
/// ([`crate::block::begin_operation`]), because that is where the device-time
/// budget for this one comes from: [`XhciController::msc_read`] recovers it and
/// [`XhciController::scsi`] is where it is honoured. A call with no
/// establishment above it is refused by name rather than served without a
/// budget.
///
/// [`BlockError::BudgetExpired`] is that refusal and
/// [`BlockError::Device`] is everything else — including no disk under this
/// index, which the controller has nothing to ask about.
pub fn storage_read(index: usize, lba: u64, count: u32, buf: &mut [u8]) -> BlockResult {
    with_disk(index, |ctrl, local| ctrl.msc_read(local, lba, count, buf))
        .unwrap_or(Err(BlockError::Device))
}

pub fn storage_write(index: usize, lba: u64, count: u32, buf: &[u8]) -> BlockResult {
    with_disk(index, |ctrl, local| ctrl.msc_write(local, lba, count, buf))
        .unwrap_or(Err(BlockError::Device))
}

pub fn storage_flush(index: usize) -> BlockResult {
    with_disk(index, |ctrl, local| ctrl.msc_flush(local)).unwrap_or(Err(BlockError::Device))
}

