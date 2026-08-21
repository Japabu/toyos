//! NVMe, as a [`BlockDevice`].
//!
//! **One command outstanding at a time**, which is a property of this driver
//! and not of the protocol: every submission goes through `submit_and_wait`, so
//! the completion at the head of a queue is always the one the caller asked
//! for. `wait_completion`'s `cid` comparison is what checks that rather than
//! assuming it.
//!
//! # Two bounds, and only one of them is this driver's
//!
//! [`COMMAND`] bounds *one* command, and it is reached only by a controller
//! that has stopped answering. What a caller actually spends is the composition
//! above it — one `read_blocks` of N blocks is `ceil(N / 32)` commands — and
//! nothing in this driver has an opinion about how long that may be.
//! [`crate::block::OPERATION`] is that opinion, and it belongs to the layer
//! that knows one call is one operation.
//!
//! **It arrives ambiently and is threaded from there.** Owner ruling 1B: the
//! deadline is established on the running context by
//! [`crate::block::begin_operation`] in [`NvmeBlockDevice`]'s trait methods —
//! this file is both the establisher and the driver, where the USB path needs
//! two files for it — recovered by `read_blocks` and `write_blocks`, and from
//! there an ordinary argument down to `read_sectors` and `write_sectors`, which
//! are the two sites that read it. `admin` is deliberately outside that: it is
//! reached only from [`init`], bringing a controller up is not a block-device
//! operation and has no establishment above it, so it takes no deadline
//! argument and asking for one would panic the boot by name. What bounds it is
//! [`COMMAND`], like every other command here.
//!
//! **The refusal is taken between commands and never inside one**, for the
//! reason `XhciController::scsi` states at length: ending a wait at the
//! caller's deadline abandons a command the device is still going to answer.
//! Here that costs more than it does there, because there is no reset in this
//! driver to take it back — see [`COMMAND`].

use core::ptr::{read_volatile, write_volatile, write_bytes, copy_nonoverlapping};
use core::sync::atomic::{fence, Ordering};
use toyos_untrusted::{Refused, Untrusted};
use crate::mm::Mmio;
use super::pci::PciDevice;
use super::DmaPool;
use crate::block::{self, BlockDevice, BlockError, BlockResult, DeviceId};
use crate::mm::paging::CachePolicy;
use crate::log;
use crate::mm::KernelSlice;
use crate::scheduler::Operation;
use crate::sync::Lock;
use crate::time::{Budget, Deadline, Duration};

// NVMe register offsets (BAR0 MMIO)
const REG_CAP: u64 = 0x00;
const REG_CC: u64 = 0x14;
const REG_CSTS: u64 = 0x1C;
const REG_AQA: u64 = 0x24;
const REG_ASQ: u64 = 0x28;
const REG_ACQ: u64 = 0x30;

const QUEUE_DEPTH: usize = 16;

/// How long one command may spend in the controller before this driver stops
/// believing a completion is coming.
///
/// **The number is not chosen here; it is the term
/// [`crate::block::OPERATION`]'s own derivation already spends.** That budget
/// is two seconds because "the refusal is taken between commands and never
/// inside one, so the overshoot is the command in flight — one more transfer
/// bound at worst — and `2 + 2` leaves a second of the daemon's 5 s for it to
/// notice with". `xhci`'s `USB_TIMEOUT_NS` is that bound on the USB path; this
/// is it on this one, and it is the same number because the arithmetic above it
/// is the same arithmetic. It is generous by construction: an I/O command
/// completes in microseconds even under TCG, so nothing but a controller that
/// has stopped answering reaches it.
///
/// **A [`Budget`] and not a [`crate::time::Bound`].** NVMe 2.0 states no
/// completion timeout for an I/O command; `CAP.TO` is the one number the device
/// publishes about waiting and it bounds exactly the `CSTS.RDY` transitions in
/// [`init`], which are a different wait and a different chunk
/// (`issues/kernel/driver-waits-without-a-deadline.md` owns those two, and they
/// still spin unbounded).
///
/// **Its expiry ends this controller, which is why it may be generous.** A
/// command this driver stops waiting for is a command the device still owns:
/// its PRP list still names the shared DMA window and its completion still owes
/// the entry at `cq_head`, so a command issued after it would race a stranger's
/// DMA and read a stranger's status. There is no controller reset here to take
/// either back, so the queue is abandoned with the command.
const COMMAND: Budget = Budget::of(
    Duration::from_secs(2),
    "the command is abandoned, the controller is marked failed, and every later \
     operation on this disk is refused",
);

/// NVMe Identify Namespace data structure (partial — only fields we use).
#[repr(C)]
struct IdentifyNamespace {
    nsze: u64,            // offset 0: namespace size in LBAs
    ncap: u64,            // offset 8: namespace capacity
    nuse: u64,            // offset 16: namespace utilization
    nsfeat: u8,           // offset 24
    nlbaf: u8,            // offset 25: number of LBA formats (0-based)
    flbas: u8,            // offset 26: formatted LBA size
    _padding: [u8; 101],  // offsets 27..128
    lba_formats: [u32; 64], // offset 128: LBA format descriptors (4 bytes each)
}

const ADMIN_CREATE_IO_SQ: u8 = 0x01;
const ADMIN_CREATE_IO_CQ: u8 = 0x05;
const ADMIN_IDENTIFY: u8 = 0x06;
const IO_WRITE: u8 = 0x01;
const IO_READ: u8 = 0x02;

#[repr(C)]
#[derive(Clone, Copy)]
struct SqEntry {
    cdw0: u32,
    nsid: u32,
    cdw2: u32,
    cdw3: u32,
    mptr: u64,
    prp1: u64,
    prp2: u64,
    cdw10: u32,
    cdw11: u32,
    cdw12: u32,
    cdw13: u32,
    cdw14: u32,
    cdw15: u32,
}

impl SqEntry {
    const ZERO: Self = Self {
        cdw0: 0, nsid: 0, cdw2: 0, cdw3: 0,
        mptr: 0, prp1: 0, prp2: 0,
        cdw10: 0, cdw11: 0, cdw12: 0, cdw13: 0, cdw14: 0, cdw15: 0,
    };
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CqEntry {
    dw0: u32,
    dw1: u32,
    sq_head: u16,
    sq_id: u16,
    cid: u16,
    status: u16, // bit 0 = phase, bits [15:1] = status
}

struct NvmeQueue {
    sq: *mut SqEntry,
    cq: *mut CqEntry,
    sq_tail: u16,
    cq_head: u16,
    phase: bool,
    sq_doorbell: u64,
    cq_doorbell: u64,
}

impl NvmeQueue {
    fn new(sq: *mut SqEntry, cq: *mut CqEntry, qid: u16, stride: u32) -> Self {
        let doorbell_stride = 4u64 << stride;
        Self {
            sq, cq,
            sq_tail: 0, cq_head: 0, phase: true,
            sq_doorbell: 0x1000 + (2 * qid as u64) * doorbell_stride,
            cq_doorbell: 0x1000 + (2 * qid as u64 + 1) * doorbell_stride,
        }
    }

    fn submit(&mut self, bar: &Mmio, cmd: SqEntry) {
        unsafe { write_volatile(self.sq.add(self.sq_tail as usize), cmd); }
        self.sq_tail = (self.sq_tail + 1) % QUEUE_DEPTH as u16;
        fence(Ordering::Release);
        bar.write_u32(self.sq_doorbell, self.sq_tail as u32);
    }

    /// Wait for the completion at the head of the queue, and refuse it unless
    /// its `cid` is `expected`.
    ///
    /// `cid` is the one number this driver chose for the command it
    /// submitted and the device must echo back unchanged (NVMe 2.0
    /// §3.3.3.2.1); nothing compared it against anything until now. Sound
    /// today only because every submission on this queue is synchronous —
    /// one command outstanding at a time is a property of the caller, not of
    /// this parse, which is exactly why the comparison belongs here rather
    /// than staying an invariant nobody checks.
    ///
    /// **Bounded by [`COMMAND`], and by nothing the caller chose.** This loop
    /// used to have no deadline in it at all, which mattered more here than
    /// anywhere else in the kernel: every real caller reaches it holding
    /// `page_cache::BLOCK_CACHE` *and* `page_cache::BLOCK_DEV`, both
    /// `sync::Lock`s that disable preemption for their whole life, so a
    /// controller that stopped answering wedged a CPU holding two of the
    /// machine's statics and the only thing that ever said so was some other
    /// CPU's `DEADLOCK` panic naming the victim.
    ///
    /// **Two reads of the entry and not one.** [`crate::clock::settles`] is the
    /// kernel's one bounded driver spin and it takes a predicate, so the read
    /// that decides is not the read that is consumed. Sound because one command
    /// is outstanding at a time: once the phase bit at `cq_head` has flipped,
    /// nothing writes that entry again until the head has been the whole way
    /// round the queue. Spelling the loop out to read once instead would be a
    /// fourth copy of `settles`' body, and that function's own doc records why
    /// the body may not read `nanos_since_boot` per iteration.
    fn wait_completion(&mut self, bar: &Mmio, expected: u16) -> Result<u16, Unanswered> {
        let (cq, head, phase) = (self.cq, self.cq_head, self.phase);
        let answered = crate::clock::settles(COMMAND.nanos(), || {
            let entry = unsafe { read_volatile(cq.add(head as usize)) };
            ((entry.status & 1) != 0) == phase
        });
        if !answered {
            return Err(Unanswered::Silent);
        }
        let cq = unsafe { read_volatile(self.cq.add(self.cq_head as usize)) };
        let status = cq.status >> 1;
        let cid = Untrusted::new(cq.cid);
        self.cq_head = (self.cq_head + 1) % QUEUE_DEPTH as u16;
        if self.cq_head == 0 {
            self.phase = !self.phase;
        }
        bar.write_u32(self.cq_doorbell, self.cq_head as u32);
        cid.exactly(expected).map(|_| status).map_err(Unanswered::Wrong)
    }

    fn submit_and_wait(&mut self, bar: &Mmio, cmd: SqEntry) -> Result<u16, Unanswered> {
        // The cid this driver chose for `cmd` is already packed into its own
        // dword, which is why `wait_completion` needs no argument beyond it:
        // reading it back out is not trusting `cmd` again, it is naming what
        // this call itself just wrote.
        let expected = (cmd.cdw0 >> 16) as u16;
        self.submit(bar, cmd);
        self.wait_completion(bar, expected)
    }
}

/// Why a submitted command produced no status this driver may use.
///
/// Two arms and not one, because what they leave behind differs and the
/// controller's fate is decided on that difference. A completion carrying the
/// wrong `cid` leaves the queue *consistent* — the entry was consumed, the head
/// advanced, the doorbell rang — so the next command starts from a known place.
/// A command that was never answered leaves the queue owed an entry and the DMA
/// window owed a write, and nothing in this driver can take either back.
enum Unanswered {
    /// The completion queue answered a different command.
    Wrong(Refused),
    /// The controller did not answer inside [`COMMAND`].
    Silent,
}

impl core::fmt::Display for Unanswered {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Wrong(refused) => {
                write!(f, "the completion queue answered a different command ({refused})")
            }
            Self::Silent => write!(f, "no completion in {}", COMMAND.duration()),
        }
    }
}

// DMA layout (byte offsets)
const OFF_ADMIN_SQ: usize   = 0x0000;
const OFF_ADMIN_CQ: usize   = 0x1000;
const OFF_IO_SQ: usize      = 0x2000;
const OFF_IO_CQ: usize      = 0x3000;
const OFF_IDENTIFY: usize   = 0x4000;
const OFF_PRP_LIST: usize   = 0x5000;
const OFF_DATA: usize       = 0x6000;
const MAX_DATA_PAGES: usize  = 32;
const DMA_SIZE: usize        = OFF_DATA + MAX_DATA_PAGES * 0x1000;

static DMA_POOL: Lock<Option<DmaPool>> = Lock::new(None);

fn dma() -> KernelSlice {
    DMA_POOL.lock().as_ref().unwrap().slice()
}

struct NvmeController {
    bar: Mmio,
    admin: NvmeQueue,
    io: NvmeQueue,
    next_cid: u16,
    sector_size: u32,
    ns_size: u64,
    /// Whether a command has been abandoned on this controller. Once it has,
    /// the queues and the DMA window are the device's and this driver issues
    /// nothing more on them — see [`COMMAND`].
    failed: bool,
}

impl NvmeController {
    fn alloc_cid(&mut self) -> u16 {
        let cid = self.next_cid;
        self.next_cid = self.next_cid.wrapping_add(1);
        cid
    }

    /// One command on each queue, with the one verdict that outlives the
    /// command folded into the controller's own state.
    fn admin_command(&mut self, cmd: SqEntry) -> Result<u16, Unanswered> {
        let out = self.admin.submit_and_wait(&self.bar, cmd);
        self.note(&out);
        out
    }

    fn io_command(&mut self, cmd: SqEntry) -> Result<u16, Unanswered> {
        let out = self.io.submit_and_wait(&self.bar, cmd);
        self.note(&out);
        out
    }

    /// A command nobody answered ends this controller, once and loudly.
    ///
    /// Once, because the line is about the abandonment and not about the caller
    /// that noticed: the page cache retries, and a line per refused operation
    /// would bury the one that says what happened. Every later refusal is
    /// silent by design and carries the caller's own log line above it.
    fn note(&mut self, out: &Result<u16, Unanswered>) {
        if matches!(out, Err(Unanswered::Silent)) && !self.failed {
            self.failed = true;
            log!("NVMe: this controller is offline: the command it did not answer still owns \
                 its PRP list and is still owed a completion entry, and this driver has no \
                 reset to take either back");
        }
    }

    /// An admin command, with the status the controller returned actually
    /// looked at. Six calls here discarded it, so a controller that refused to
    /// identify itself or to create a queue produced a driver that went on to
    /// read whatever the DMA buffer held and derive a geometry from it.
    ///
    /// No deadline argument, and no establishment above it: bringing a
    /// controller up is not a block-device operation, so what bounds these is
    /// [`COMMAND`] alone.
    fn admin(&mut self, cmd: SqEntry, what: &str) -> bool {
        let status = match self.admin_command(cmd) {
            Ok(status) => status,
            Err(why) => {
                log!("NVMe: {what}: {why}");
                return false;
            }
        };
        if status != 0 {
            log!("NVMe: {what} failed, status={status:#x}");
            return false;
        }
        true
    }

    /// Whether this command may be issued at all: the controller still has its
    /// queues, and the caller's budget has something left in it.
    ///
    /// **Read between commands and never inside one**, which is the whole of
    /// why a refusal here is free. Nothing has been submitted, no completion is
    /// owed and the DMA window is nobody's, so this is a decision about the
    /// *caller's* time and never a verdict about the disk: the controller is
    /// left exactly as the previous operation left it, and the next caller
    /// finds it that way. [`crate::block::OPERATION`] carries the rest of the
    /// argument, and `XhciController::scsi` is the same decision on the USB
    /// path.
    ///
    /// An offline controller refuses silently: the line that says what happened
    /// was written once by [`Self::note`], and one per refused command after it
    /// would bury that line under the page cache's retries.
    fn may_issue(&self, until: Deadline, op: &str, lba: u64, sector_count: u32) -> bool {
        if self.failed {
            return false;
        }
        if until.reached(crate::clock::now()) {
            log!("NVMe: {op} of {sector_count} sectors at {lba} not issued: {}", block::OPERATION);
            return false;
        }
        true
    }

    fn identify_controller(&mut self) -> bool {
        let dma = dma();
        let cid = self.alloc_cid();
        let mut cmd = SqEntry::ZERO;
        cmd.cdw0 = (cid as u32) << 16 | ADMIN_IDENTIFY as u32;
        cmd.prp1 = dma.phys() + OFF_IDENTIFY as u64;
        cmd.cdw10 = 1;
        self.admin(cmd, "Identify Controller")
    }

    fn create_io_cq(&mut self) -> bool {
        unsafe { write_bytes(self.io.cq as *mut u8, 0, QUEUE_DEPTH * core::mem::size_of::<CqEntry>()); }
        let dma = dma();
        let cid = self.alloc_cid();
        let mut cmd = SqEntry::ZERO;
        cmd.cdw0 = (cid as u32) << 16 | ADMIN_CREATE_IO_CQ as u32;
        cmd.prp1 = dma.phys() + OFF_IO_CQ as u64;
        cmd.cdw10 = ((QUEUE_DEPTH as u32 - 1) << 16) | 1;
        cmd.cdw11 = 1;
        self.admin(cmd, "Create I/O Completion Queue")
    }

    fn create_io_sq(&mut self) -> bool {
        unsafe { write_bytes(self.io.sq as *mut u8, 0, QUEUE_DEPTH * core::mem::size_of::<SqEntry>()); }
        let dma = dma();
        let cid = self.alloc_cid();
        let mut cmd = SqEntry::ZERO;
        cmd.cdw0 = (cid as u32) << 16 | ADMIN_CREATE_IO_SQ as u32;
        cmd.prp1 = dma.phys() + OFF_IO_SQ as u64;
        cmd.cdw10 = ((QUEUE_DEPTH as u32 - 1) << 16) | 1;
        cmd.cdw11 = (1 << 16) | 1;
        self.admin(cmd, "Create I/O Submission Queue")
    }

    fn identify_namespace(&mut self) -> bool {
        let dma = dma();
        let identify_ptr = dma.ptr_at(OFF_IDENTIFY);
        unsafe { write_bytes(identify_ptr, 0, 4096); }
        let cid = self.alloc_cid();
        let mut cmd = SqEntry::ZERO;
        cmd.cdw0 = (cid as u32) << 16 | ADMIN_IDENTIFY as u32;
        cmd.nsid = 1;
        cmd.prp1 = dma.phys() + OFF_IDENTIFY as u64;
        cmd.cdw10 = 0;
        if !self.admin(cmd, "Identify Namespace") {
            return false;
        }

        let ns = unsafe { &*(identify_ptr as *const IdentifyNamespace) };
        let fmt_idx = (ns.flbas & 0x0F) as usize;
        let lba_ds = (ns.lba_formats[fmt_idx] >> 16) & 0xFF;
        // `lba_ds` is an 8-bit device-reported shift, and it reaches both a
        // shift and a divisor: `1 << lba_ds` overflows above 31, and above 12
        // `4096 / sector_size` is zero, which `NvmeBlockDevice::new` then
        // divides `nsze` by. Measured on QEMU 11.0.2 with
        // `nvme-ns,logical_block_size=8192`: `#DE` at `NvmeBlockDevice::new`,
        // before storage is up, on a machine with nothing to report it on.
        //
        // 512..4096 is not a policy number, it is this driver: every path
        // above the sector layer is written in 4096-byte blocks and needs the
        // sector size to divide one. A namespace outside it is unimplemented,
        // and says so with the value it reported.
        assert!(
            (9..=12).contains(&lba_ds),
            "NVMe: namespace reports 2^{lba_ds}-byte sectors (flbas={:#x}, format {fmt_idx}); \
             this driver serves 4096-byte blocks and needs 512..=4096",
            ns.flbas,
        );
        self.sector_size = 1 << lba_ds;
        self.ns_size = ns.nsze;
        log!("NVMe: NS1 size={} sectors, sector_size={}", ns.nsze, self.sector_size);
        true
    }

    /// Read `sector_count` contiguous sectors starting at `lba` into `buf`.
    /// Handles PRP list setup for multi-page transfers.
    ///
    /// `until` is the whole operation's deadline and not this command's; see
    /// [`Self::may_issue`] and the module header.
    fn read_sectors(
        &mut self,
        lba: u64,
        sector_count: u32,
        buf: &mut [u8],
        until: Deadline,
    ) -> BlockResult {
        let total_bytes = sector_count as usize * self.sector_size as usize;
        assert!(buf.len() >= total_bytes);
        assert!(total_bytes <= MAX_DATA_PAGES * 4096);

        if !self.may_issue(until, "read", lba, sector_count) {
            return Err(BlockError);
        }

        let dma = dma();
        let pages = total_bytes.div_ceil(4096);
        let data_phys = dma.phys() + OFF_DATA as u64;

        let cid = self.alloc_cid();
        let mut cmd = SqEntry::ZERO;
        cmd.cdw0 = (cid as u32) << 16 | IO_READ as u32;
        cmd.nsid = 1;
        cmd.prp1 = data_phys;
        cmd.cdw10 = lba as u32;
        cmd.cdw11 = (lba >> 32) as u32;
        cmd.cdw12 = sector_count - 1;

        if pages == 2 {
            cmd.prp2 = data_phys + 0x1000;
        } else if pages > 2 {
            let prp_list = dma.ptr_at(OFF_PRP_LIST) as *mut u64;
            for i in 1..pages {
                unsafe { prp_list.add(i - 1).write(data_phys + i as u64 * 0x1000); }
            }
            cmd.prp2 = dma.phys() + OFF_PRP_LIST as u64;
        }

        let status = match self.io_command(cmd) {
            Ok(status) => status,
            Err(why) => {
                log!("NVMe: read of {sector_count} sectors at {lba}: {why}");
                return Err(BlockError);
            }
        };
        if status != 0 {
            log!("NVMe: read of {sector_count} sectors at {lba} failed, status={status:#x}");
            return Err(BlockError);
        }

        unsafe { copy_nonoverlapping(dma.ptr_at(OFF_DATA) as *const u8, buf.as_mut_ptr(), total_bytes); }
        Ok(())
    }

    fn write_sectors(
        &mut self,
        lba: u64,
        sector_count: u32,
        buf: &[u8],
        until: Deadline,
    ) -> BlockResult {
        let total_bytes = sector_count as usize * self.sector_size as usize;
        assert!(buf.len() >= total_bytes);
        assert!(total_bytes <= MAX_DATA_PAGES * 4096);

        if !self.may_issue(until, "write", lba, sector_count) {
            return Err(BlockError);
        }

        let dma = dma();
        let pages = total_bytes.div_ceil(4096);
        let data_phys = dma.phys() + OFF_DATA as u64;

        unsafe { copy_nonoverlapping(buf.as_ptr(), dma.ptr_at(OFF_DATA), total_bytes); }

        let cid = self.alloc_cid();
        let mut cmd = SqEntry::ZERO;
        cmd.cdw0 = (cid as u32) << 16 | IO_WRITE as u32;
        cmd.nsid = 1;
        cmd.prp1 = data_phys;
        cmd.cdw10 = lba as u32;
        cmd.cdw11 = (lba >> 32) as u32;
        cmd.cdw12 = sector_count - 1;

        if pages == 2 {
            cmd.prp2 = data_phys + 0x1000;
        } else if pages > 2 {
            let prp_list = dma.ptr_at(OFF_PRP_LIST) as *mut u64;
            for i in 1..pages {
                unsafe { prp_list.add(i - 1).write(data_phys + i as u64 * 0x1000); }
            }
            cmd.prp2 = dma.phys() + OFF_PRP_LIST as u64;
        }

        let status = match self.io_command(cmd) {
            Ok(status) => status,
            Err(why) => {
                log!("NVMe: write of {sector_count} sectors at {lba}: {why}");
                return Err(BlockError);
            }
        };
        if status != 0 {
            log!("NVMe: write of {sector_count} sectors at {lba} failed, status={status:#x}");
            return Err(BlockError);
        }
        Ok(())
    }
}

/// NVMe block device exposing 4KB block I/O through the BlockDevice trait.
///
/// # Safety
/// Raw pointers in NvmeController point to DMA memory owned by this device.
/// The device is only accessed by a single owner (the VFS root filesystem).
unsafe impl Send for NvmeBlockDevice {}

pub struct NvmeBlockDevice {
    ctrl: NvmeController,
    id: DeviceId,
    sectors_per_block: u32,
    block_count: u64,
}

impl NvmeBlockDevice {
    fn new(ctrl: NvmeController, id: DeviceId) -> Self {
        let sectors_per_block = 4096 / ctrl.sector_size;
        let block_count = ctrl.ns_size / sectors_per_block as u64;
        log!("NVMe: block device id={} blocks={} ({}MB)",
            id, block_count, block_count * 4096 / (1024 * 1024));
        Self { ctrl, id, sectors_per_block, block_count }
    }

    /// The namespace's own logical block size. `BlockDevice` deliberately
    /// hides it — everything above this driver is written in 4 KiB blocks —
    /// but a GPT is laid out in the device's blocks and in nothing else, so
    /// the one caller that has to speak the device's units asks here.
    pub fn sector_size(&self) -> u32 {
        self.ctrl.sector_size
    }
}

impl BlockDevice for NvmeBlockDevice {
    fn device_id(&self) -> DeviceId { self.id }
    fn block_count(&self) -> u64 { self.block_count }

    /// The guard is a `let _op` and not a `let _`: `let _` drops at the end of
    /// its statement, which would end the operation before the loop it bounds.
    /// [`Operation::deadline`] is read *after* the establishment because an
    /// inner establishment may only narrow — a caller that arrived with less
    /// than two seconds left keeps its own deadline, and that is the value the
    /// batching loop below spends.
    fn read_blocks(&mut self, lba: u64, count: u32, buf: &mut [u8]) -> BlockResult {
        assert_eq!(buf.len(), count as usize * 4096);
        let _op = block::begin_operation();
        let until = Operation::deadline();
        let mut remaining = count;
        let mut block = lba;
        let mut offset = 0usize;

        while remaining > 0 {
            let batch = remaining.min(MAX_DATA_PAGES as u32);
            let sector_lba = block * self.sectors_per_block as u64;
            let sector_count = batch * self.sectors_per_block;
            let bytes = batch as usize * 4096;

            self.ctrl
                .read_sectors(sector_lba, sector_count, &mut buf[offset..offset + bytes], until)?;

            block += batch as u64;
            offset += bytes;
            remaining -= batch;
        }
        Ok(())
    }

    fn write_blocks(&mut self, lba: u64, count: u32, buf: &[u8]) -> BlockResult {
        assert_eq!(buf.len(), count as usize * 4096);
        let _op = block::begin_operation();
        let until = Operation::deadline();
        let mut remaining = count;
        let mut block = lba;
        let mut offset = 0usize;

        while remaining > 0 {
            let batch = remaining.min(MAX_DATA_PAGES as u32);
            let sector_lba = block * self.sectors_per_block as u64;
            let sector_count = batch * self.sectors_per_block;
            let bytes = batch as usize * 4096;

            self.ctrl
                .write_sectors(sector_lba, sector_count, &buf[offset..offset + bytes], until)?;

            block += batch as u64;
            offset += bytes;
            remaining -= batch;
        }
        Ok(())
    }

    /// NVMe writes are synchronous (`submit_and_wait`), so data is on disk
    /// after `write_blocks` returns. Nothing to flush — and so nothing to
    /// bound, which is why this is the one trait method here that establishes
    /// no operation: it issues no command and cannot spend a caller's time.
    ///
    /// A controller that has been abandoned is the exception, and it is not a
    /// flush that failed. The writes this would have made durable are the ones
    /// that never completed, and answering `Ok` would tell `page_cache::sync`
    /// they had.
    fn flush(&mut self) -> BlockResult {
        if self.ctrl.failed {
            return Err(BlockError);
        }
        Ok(())
    }
}

/// Bring up the machine's NVMe controller.
///
/// The first one, and a machine with two loses the second: unlike xHCI, where
/// the second controller is where a Tiger Lake laptop's keyboard actually is,
/// nothing above here can hold more than one disk yet — `page_cache::init`
/// takes a single `BlockDevice`. Filed rather than papered over.
pub fn init(devices: &[PciDevice]) -> Option<NvmeBlockDevice> {
    let pci_dev = *devices.iter().find(|d| d.matches_class(0x01, 0x08, None))?;
    log!("NVMe: found at PCI {:02x}:{:02x}.{}", pci_dev.bus, pci_dev.dev, pci_dev.func);
    *DMA_POOL.lock() = Some(DmaPool::alloc(DMA_SIZE));

    // A refusal rather than a panic, like this driver's existing one for a
    // sector size it cannot serve: a machine whose NVMe function publishes
    // something other than a memory BAR 0 has no disk this driver can drive,
    // and it still boots and still says why. NVMe 2.0 §3.1 requires BAR 0 to be
    // memory, so this is a controller disagreeing with its own specification.
    let bar_addr = match pci_dev.memory_bar(0) {
        Ok(memory) => memory.address(),
        Err(why) => {
            log!("NVMe: NOT INITIALISED at PCI {:02x}:{:02x}.{} — its registers are in BAR 0 and \
                 {}", pci_dev.bus, pci_dev.dev, pci_dev.func, why);
            return None;
        }
    };
    pci_dev.enable_bus_master();
    log!("NVMe: BAR0={:#x}", bar_addr);

    let bar = crate::mm::paging::map_mmio(bar_addr, 0x4000, CachePolicy::DeferToMtrr);

    let cap = bar.read_u64(REG_CAP);
    let stride = ((cap >> 32) & 0xF) as u32;

    let cc = bar.read_u32(REG_CC);
    if cc & 1 != 0 {
        bar.write_u32(REG_CC, cc & !1);
        while bar.read_u32(REG_CSTS) & 1 != 0 {
            core::hint::spin_loop();
        }
    }

    let dma = dma();
    let admin_sq = dma.ptr_at(OFF_ADMIN_SQ) as *mut SqEntry;
    let admin_cq = dma.ptr_at(OFF_ADMIN_CQ) as *mut CqEntry;
    let io_sq = dma.ptr_at(OFF_IO_SQ) as *mut SqEntry;
    let io_cq = dma.ptr_at(OFF_IO_CQ) as *mut CqEntry;

    unsafe {
        write_bytes(admin_sq as *mut u8, 0, 4096);
        write_bytes(admin_cq as *mut u8, 0, 4096);
    }

    let aqa = ((QUEUE_DEPTH as u32 - 1) << 16) | (QUEUE_DEPTH as u32 - 1);
    bar.write_u32(REG_AQA, aqa);
    bar.write_u64(REG_ASQ, dma.phys() + OFF_ADMIN_SQ as u64);
    bar.write_u64(REG_ACQ, dma.phys() + OFF_ADMIN_CQ as u64);

    let cc = 1 | (6 << 16) | (4 << 20);
    bar.write_u32(REG_CC, cc);

    while bar.read_u32(REG_CSTS) & 1 == 0 {
        core::hint::spin_loop();
    }
    log!("NVMe: controller enabled");

    let mut ctrl = NvmeController {
        bar,
        admin: NvmeQueue::new(admin_sq, admin_cq, 0, stride),
        io: NvmeQueue::new(io_sq, io_cq, 1, stride),
        next_cid: 0,
        sector_size: 512,
        ns_size: 0,
        failed: false,
    };

    // A controller that refuses any of these has not given the driver a
    // namespace to serve. Going on regardless is what discarding the statuses
    // amounted to: `identify_namespace` would read a zeroed DMA buffer and
    // derive its geometry from it.
    if !ctrl.identify_controller()
        || !ctrl.create_io_cq()
        || !ctrl.create_io_sq()
        || !ctrl.identify_namespace()
    {
        log!("NVMe: controller did not come up; this machine has no NVMe storage");
        return None;
    }

    Some(NvmeBlockDevice::new(ctrl, 1))
}
