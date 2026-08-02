//! USB Mass Storage, Bulk-Only Transport with a transparent SCSI command set
//! (interface class 0x08, subclass 0x06, protocol 0x50).
//!
//! Everything that arrives here came off a wire, so nothing in this file may
//! panic on it: a capacity, a block size, a CSW tag and a residue are all
//! numbers a broken or hostile device chooses. They are checked and the device
//! is refused by name — never truncated to fit, and never trusted because the
//! transfer that carried them succeeded.

use core::ptr::{copy_nonoverlapping, write_bytes};

use crate::log;
use super::device::Endpoint;
use super::{Trb, TrbRing, XhciController, StorageGeometry, PAGE};
use super::{CC_SUCCESS, CC_STALL, CC_SHORT_PACKET, TRB_NORMAL, TRB_CONFIGURE_EP};
use super::{TRB_RESET_ENDPOINT, TRB_SET_TR_DEQUEUE, OFF_INPUT_CTX};
use super::{MSC_IN_RING, MSC_OUT_RING, MSC_CBW, MSC_CSW, MSC_SCRATCH, MSC_SCRATCH_LEN};
use super::{MSC_DATA, MSC_DATA_LEN, MSC_MAX_BLOCKS};

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
const READY_BUDGET_NS: u64 = 500_000_000;

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
    pub fn online(&self) -> bool {
        !self.failed
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

/// How one Bulk-Only round trip ended.
enum Bot {
    /// CSW status 0. `residue` is what the device did not move, which for a
    /// READ or WRITE means the command did less than it was asked.
    Done { residue: u32 },
    /// CSW status 1: the device understood and refused. Sense data says why.
    Failed,
    /// The device must be put back together before the next command: a phase
    /// error, a malformed or mistagged CSW, a stall the endpoint reset did not
    /// clear, or silence.
    Broken,
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
}

impl Scsi {
    /// SBC's ILLEGAL REQUEST / INVALID COMMAND OPERATION CODE: the device does
    /// not have this opcode. For a command SBC makes optional that is an
    /// answer and not a failure.
    fn unimplemented(&self) -> bool {
        matches!(self, Self::Refused { key: 0x05, asc: 0x20, ascq: 0x00 })
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
const FLUSH_SENSE: Option<(u8, u8, u8)> = if cfg!(feature = "usb-flush-unimplemented") {
    Some((0x05, 0x20, 0x00))
} else if cfg!(feature = "usb-flush-fails") {
    Some((0x04, 0x44, 0x00))
} else {
    None
};

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
    /// Run `f` against the `index`-th disk, writing the device's state back
    /// whatever `f` did with it.
    fn with_storage<R>(
        &mut self,
        index: usize,
        f: impl FnOnce(&mut Self, &mut MscDevice) -> R,
    ) -> Option<R> {
        let mut dev = *self.storage.get(index)?;
        let out = f(self, &mut dev);
        self.storage[index] = dev;
        Some(out)
    }

    pub(super) fn msc_read(&mut self, index: usize, lba: u64, count: u32, buf: &mut [u8]) -> bool {
        self.with_storage(index, |ctrl, dev| {
            ctrl.transfer_blocks(dev, lba, count, Host::Into(buf))
        })
        .unwrap_or(false)
    }

    pub(super) fn msc_write(&mut self, index: usize, lba: u64, count: u32, buf: &[u8]) -> bool {
        self.with_storage(index, |ctrl, dev| {
            ctrl.transfer_blocks(dev, lba, count, Host::From(buf))
        })
        .unwrap_or(false)
    }

    pub(super) fn msc_flush(&mut self, index: usize) -> bool {
        let disk = self.disk_base + index;
        self.with_storage(index, |ctrl, dev| {
            if dev.failed {
                return false;
            }
            // LBA 0, block count 0: the whole medium, which is the only thing
            // a cache flush above a block device can mean.
            let cdb = [0x35u8, 0, 0, 0, 0, 0, 0, 0, 0, 0];
            let issued = ctrl.scsi(dev, &cdb, 10, 0, 0, false);
            let outcome = match FLUSH_SENSE {
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
                    log!("usb-storage: disk {disk} does not implement SYNCHRONIZE CACHE \
                         (sense 0x05/0x20/0x00); its writes are durable once they complete");
                }
                return true;
            }
            match outcome {
                Scsi::Ok { .. } => true,
                Scsi::Refused { key, asc, ascq } => {
                    log_refusal(&cdb, key, asc, ascq);
                    false
                }
                Scsi::Broken => false,
            }
        })
        .unwrap_or(false)
    }

    /// Move `count` 4 KiB blocks between the caller's buffer and the disk.
    fn transfer_blocks(
        &mut self,
        dev: &mut MscDevice,
        lba: u64,
        count: u32,
        mut host: Host<'_>,
    ) -> bool {
        let write = matches!(host, Host::From(_));
        // The caller is the kernel and the trait states this contract, so a
        // mismatch is a kernel bug and gets fail-fast. Everything below this
        // line is about the *device's* numbers, which get refusals instead.
        assert_eq!(host.len(), count as usize * HOST_BLOCK as usize);
        if dev.failed {
            return false;
        }
        if count == 0 {
            return true;
        }
        match lba.checked_add(count as u64) {
            Some(end) if end <= dev.blocks => {}
            _ => {
                log!("usb-storage: {lba}+{count} is past the {} blocks this disk has", dev.blocks);
                return false;
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
                unsafe {
                    copy_nonoverlapping(
                        src.as_ptr().add(offset),
                        dma.ptr_at(dev.block + MSC_DATA),
                        bytes,
                    );
                }
            }

            match self.scsi(dev, &cdb, 10, data_phys, bytes as u32, !write) {
                Scsi::Ok { delivered } if delivered as usize == bytes => {}
                // Short of what was asked, and reported as success. Nothing
                // above here has a way to say "these blocks arrived and those
                // did not", so a partial transfer is a failed one.
                Scsi::Ok { delivered } => {
                    log!("usb-storage: {delivered} of {bytes} B at block {}", lba + done as u64);
                    return false;
                }
                Scsi::Refused { key, asc, ascq } => {
                    log_refusal(&cdb, key, asc, ascq);
                    return false;
                }
                Scsi::Broken => return false,
            }

            if let Host::Into(dst) = &mut host {
                unsafe {
                    copy_nonoverlapping(
                        dma.ptr_at(dev.block + MSC_DATA) as *const u8,
                        dst.as_mut_ptr().add(offset),
                        bytes,
                    );
                }
            }
            done += batch;
        }
        true
    }

    /// One SCSI command, with the transport's recovery applied. `Scsi::Ok`
    /// means the device reported success and moved `delivered` bytes.
    fn scsi(
        &mut self,
        dev: &mut MscDevice,
        cdb: &[u8],
        cdb_len: u8,
        data_phys: u64,
        data_len: u32,
        data_in: bool,
    ) -> Scsi {
        match self.bot(dev, cdb, cdb_len, data_phys, data_len, data_in) {
            Bot::Done { residue } if residue <= data_len => {
                Scsi::Ok { delivered: data_len - residue }
            }
            // A residue larger than the transfer is a device contradicting
            // itself. Believing it would underflow the byte count every caller
            // then uses to decide how much of the buffer is real.
            Bot::Done { residue } => {
                log!("usb-storage: CSW claims {residue} B unmoved of {data_len}");
                Scsi::Broken
            }
            Bot::Failed => {
                let (key, asc, ascq) = self.request_sense(dev);
                Scsi::Refused { key, asc, ascq }
            }
            Bot::Broken => {
                log!("usb-storage: transport broke on SCSI {:#04x}, resetting",
                    cdb.first().copied().unwrap_or(0));
                if !self.reset_recovery(dev) {
                    log!("usb-storage: reset recovery failed; disk is offline");
                    dev.failed = true;
                }
                Scsi::Broken
            }
        }
    }

    /// REQUEST SENSE, as (sense key, ASC, ASCQ) and zeroed if the device would
    /// not say. Never a decision about the *transport*, which is what
    /// [`Bot::Broken`] covers and what recovery answers; the sense bytes decide
    /// only what the device meant by declining a command it understood, and
    /// zeroes fall on the failing side of every such decision.
    fn request_sense(&mut self, dev: &mut MscDevice) -> (u8, u8, u8) {
        let dma = self.dma();
        let phys = dma.phys() + (dev.block + MSC_SCRATCH) as u64;
        unsafe { write_bytes(dma.ptr_at(dev.block + MSC_SCRATCH), 0, MSC_SCRATCH_LEN); }
        let cdb = [0x03u8, 0, 0, 0, 18, 0];
        // Recursion is not possible: a failing REQUEST SENSE goes through
        // `bot` directly, so it cannot ask for sense data about itself.
        match self.bot(dev, &cdb, 6, phys, 18, true) {
            Bot::Done { residue } if residue <= 5 => {
                let mut resp = [0u8; 18];
                unsafe {
                    copy_nonoverlapping(
                        dma.ptr_at(dev.block + MSC_SCRATCH) as *const u8,
                        resp.as_mut_ptr(),
                        resp.len(),
                    );
                }
                (resp[2] & 0x0F, resp[12], resp[13])
            }
            _ => (0, 0, 0),
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
    ) -> Bot {
        // The CDBs are this file's own, so their shape is a kernel invariant.
        assert!(cdb_len as usize <= cdb.len() && cdb_len <= 16);
        assert!(data_len as usize <= MSC_DATA_LEN);

        let dma = self.dma();
        let tag = dev.next_tag();
        let cbw = dma.subslice(dev.block + MSC_CBW, CBW_LEN as usize);
        unsafe {
            cbw.zero();
            cbw.write::<u32>(0, CBW_SIGNATURE.to_le());
            cbw.write::<u32>(4, tag.to_le());
            cbw.write::<u32>(8, data_len.to_le());
            cbw.write::<u8>(12, if data_in { 0x80 } else { 0x00 });
            cbw.write::<u8>(13, 0); // LUN 0: this driver binds one logical unit
            cbw.write::<u8>(14, cdb_len);
            cbw.copy_from(15, &cdb[..cdb_len as usize]);
        }

        let cbw_phys = dma.phys() + (dev.block + MSC_CBW) as u64;
        match self.bulk(dev, false, cbw_phys, CBW_LEN) {
            Some((CC_SUCCESS, 0)) => {}
            _ => return Bot::Broken,
        }

        if data_len > 0 {
            match self.bulk(dev, data_in, data_phys, data_len) {
                Some((CC_SUCCESS, _)) | Some((CC_SHORT_PACKET, _)) => {}
                // A stalled data phase is ordinary — an unsupported command
                // or a read past the end stalls here — and the CSW still
                // arrives once the endpoint is unhalted. Recovering and then
                // reading the status is what turns it into a clean refusal.
                Some((CC_STALL, _)) => {
                    if !self.clear_stall(dev, data_in) {
                        return Bot::Broken;
                    }
                }
                _ => return Bot::Broken,
            }
        }

        let csw_phys = dma.phys() + (dev.block + MSC_CSW) as u64;
        unsafe { write_bytes(dma.ptr_at(dev.block + MSC_CSW), 0, CSW_LEN as usize); }
        let mut got = self.bulk(dev, true, csw_phys, CSW_LEN);
        if let Some((CC_STALL, _)) = got {
            // The spec's one legal retry: the device may stall the status
            // phase once, and a second stall means it has lost the plot.
            if !self.clear_stall(dev, true) {
                return Bot::Broken;
            }
            unsafe { write_bytes(dma.ptr_at(dev.block + MSC_CSW), 0, CSW_LEN as usize); }
            got = self.bulk(dev, true, csw_phys, CSW_LEN);
        }
        match got {
            Some((CC_SUCCESS, 0)) => {}
            _ => return Bot::Broken,
        }

        let csw = dma.subslice(dev.block + MSC_CSW, CSW_LEN as usize);
        let (signature, csw_tag, residue, status) = unsafe {
            (
                u32::from_le(csw.read::<u32>(0)),
                u32::from_le(csw.read::<u32>(4)),
                u32::from_le(csw.read::<u32>(8)),
                csw.read::<u8>(12),
            )
        };
        if signature != CSW_SIGNATURE {
            log!("usb-storage: CSW signature {signature:#010x}");
            return Bot::Broken;
        }
        // A CSW carrying somebody else's tag is a device out of step with the
        // driver, and accepting it would attribute one command's status to
        // another — the failure mode where a write reports the success of the
        // read before it.
        if csw_tag != tag {
            log!("usb-storage: CSW tag {csw_tag} for command {tag}");
            return Bot::Broken;
        }
        match status {
            0 => Bot::Done { residue },
            1 => Bot::Failed,
            _ => Bot::Broken,
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
        ring.enqueue(trb);
        let slot = dev.slot_id;
        self.ring_doorbell(slot, dci);
        self.wait_transfer(slot, dci)
    }

    /// Take one bulk endpoint from Halted back to a state that runs TRBs.
    ///
    /// Three steps and all three are load-bearing: Reset Endpoint moves the
    /// xHC's endpoint out of Halted, Set TR Dequeue tells it where to resume
    /// (the ring is rewound, because the TRBs after the stalled one belong to
    /// a transfer nobody is waiting for), and CLEAR_FEATURE(ENDPOINT_HALT)
    /// clears the condition at the *device*, which otherwise stalls the next
    /// packet exactly as it stalled this one.
    fn clear_stall(&mut self, dev: &mut MscDevice, in_dir: bool) -> bool {
        let (dci, ep_addr, ring_off) = if in_dir {
            (dev.in_dci, dev.in_ep, MSC_IN_RING)
        } else {
            (dev.out_dci, dev.out_ep, MSC_OUT_RING)
        };
        let slot = dev.slot_id as u32;

        let mut reset = Trb::ZERO;
        reset.control = TRB_RESET_ENDPOINT | (slot << 24) | ((dci as u32) << 16);
        if self.run_command(reset, "Reset Endpoint").is_none() {
            return false;
        }

        let fresh = TrbRing::init(self.dma().subslice(dev.block + ring_off, PAGE));
        let dequeue = fresh.dequeue();
        if in_dir {
            dev.in_ring = fresh;
        } else {
            dev.out_ring = fresh;
        }

        let mut set_dq = Trb::ZERO;
        set_dq.param = dequeue;
        set_dq.control = TRB_SET_TR_DEQUEUE | (slot << 24) | ((dci as u32) << 16);
        if self.run_command(set_dq, "Set TR Dequeue").is_none() {
            return false;
        }

        let slot_id = dev.slot_id;
        matches!(
            self.control_transfer(slot_id, &mut dev.ep0_ring, 0x02, 0x01, 0, ep_addr as u16, None, 0),
            Some(CC_SUCCESS)
        )
    }

    /// Bulk-Only Mass Storage Reset plus both endpoint clears: what the class
    /// specification requires after a phase error, and the only way back from
    /// a device whose command/data/status state machine no longer agrees with
    /// the driver's.
    fn reset_recovery(&mut self, dev: &mut MscDevice) -> bool {
        let slot = dev.slot_id;
        let iface = dev.iface as u16;
        let reset = matches!(
            self.control_transfer(slot, &mut dev.ep0_ring, 0x21, 0xFF, 0, iface, None, 0),
            Some(CC_SUCCESS)
        );
        // Both clears run even if the reset request itself did not land: the
        // endpoints are what the next command touches, and leaving one halted
        // because the other step failed turns a recoverable device into a
        // permanently offline one.
        let cleared_in = self.clear_stall(dev, true);
        let cleared_out = self.clear_stall(dev, false);
        reset && cleared_in && cleared_out
    }
}

/// Configure the two bulk endpoints and bring the disk up. `dev_block` is the
/// device's own block, which is where its EP0 ring already lives.
///
/// Returns whether the device joined `ctrl.storage`.
pub fn bind(
    ctrl: &mut XhciController,
    ep0_ring: TrbRing,
    slot_id: u8,
    speed: u8,
    port_idx: u8,
    info: &MscInterface,
) -> bool {
    let index = ctrl.storage.len();
    let Some(block) = ctrl.layout.msc(index) else {
        log!("usb-storage: slot {slot_id} is the {}th disk; this driver serves {}",
            index + 1, super::MSC_BLOCKS);
        return false;
    };

    let (in_dci, out_dci) = (info.in_ep.dci(), info.out_ep.dci());
    let dma = ctrl.dma();
    let in_ring = TrbRing::init(dma.subslice(block + MSC_IN_RING, PAGE));
    let out_ring = TrbRing::init(dma.subslice(block + MSC_OUT_RING, PAGE));

    let input_ctx = dma.subslice(OFF_INPUT_CTX, PAGE);
    let input_ctx_ptr = input_ctx.base();
    unsafe { input_ctx.zero(); }
    ctrl.write_ctx32(input_ctx_ptr, 0, 1, 1 | (1u32 << in_dci) | (1u32 << out_dci));
    let max_dci = in_dci.max(out_dci) as u32;
    ctrl.write_ctx32(input_ctx_ptr, 1, 0, ((speed as u32) << 20) | (max_dci << 27));
    ctrl.write_ctx32(input_ctx_ptr, 1, 1, (port_idx as u32 + 1) << 16);

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
        ctrl.write_ctx32(input_ctx_ptr, ctx, 0, 0);
        ctrl.write_ctx32(
            input_ctx_ptr,
            ctx,
            1,
            (3 << 1) | (ep_type << 3) | ((burst as u32) << 8) | ((mps as u32) << 16),
        );
        let dequeue = ring.dequeue();
        ctrl.write_ctx32(input_ctx_ptr, ctx, 2, dequeue as u32);
        ctrl.write_ctx32(input_ctx_ptr, ctx, 3, (dequeue >> 32) as u32);
        ctrl.write_ctx32(input_ctx_ptr, ctx, 4, mps as u32);
    }

    let mut configure = Trb::ZERO;
    configure.param = input_ctx.phys();
    configure.control = TRB_CONFIGURE_EP | ((slot_id as u32) << 24);
    if ctrl.run_command(configure, "Configure Endpoint (bulk)").is_none() {
        return false;
    }

    let mut dev = MscDevice {
        slot_id,
        iface: info.iface_num,
        in_ep: info.in_ep.addr,
        out_ep: info.out_ep.addr,
        in_dci,
        out_dci,
        block,
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
        return false;
    }
    // The machine-wide number, which is what `usb_storage::open` indexes by.
    // `index` is this controller's own and is what picked the pool block; on a
    // two-controller machine the two disagree, and printing the local one
    // would call two different disks "disk 0".
    log!(
        "usb-storage: disk {} ready on slot {slot_id}, {} blocks of {} B \
         ({} MiB), msc_block +{:#x}",
        ctrl.disk_base + index,
        dev.blocks,
        dev.logical_block_bytes,
        dev.blocks * HOST_BLOCK as u64 / (1024 * 1024),
        block
    );
    ctrl.storage.push(dev);
    true
}

/// TEST UNIT READY, INQUIRY and READ CAPACITY: everything between a configured
/// interface and a disk with a size.
fn bring_up(ctrl: &mut XhciController, dev: &mut MscDevice) -> bool {
    // Driving the transport rather than `scsi` here, for two reasons: a device
    // that answers NOT READY is expected rather than an error, so it must not
    // produce a log line per attempt, and the sense fetch that reports it is
    // also what clears the condition on a stick still spinning up.
    let give_up = crate::clock::nanos_since_boot() + READY_BUDGET_NS;
    let mut sense = (0u8, 0u8, 0u8);
    let mut ready = false;
    loop {
        match ctrl.bot(dev, &[0x00u8; 6], 6, 0, 0, false) {
            Bot::Done { .. } => {
                ready = true;
                break;
            }
            Bot::Failed => sense = ctrl.request_sense(dev),
            Bot::Broken => {
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
    let read_scratch = |ctrl: &mut XhciController,
                        dev: &mut MscDevice,
                        cdb: &[u8],
                        cdb_len: u8,
                        want: u32,
                        out: &mut [u8]| {
        unsafe { write_bytes(dma.ptr_at(dev.block + MSC_SCRATCH), 0, MSC_SCRATCH_LEN); }
        match ctrl.scsi(dev, cdb, cdb_len, scratch_phys, want, true) {
            Scsi::Ok { delivered } if delivered as usize >= out.len() => {
                unsafe {
                    copy_nonoverlapping(
                        dma.ptr_at(dev.block + MSC_SCRATCH) as *const u8,
                        out.as_mut_ptr(),
                        out.len(),
                    );
                }
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
