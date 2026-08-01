mod device;
mod hid;
mod legacy;
mod msc;

use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};
use crate::mm::Mmio;
use super::pci::PciDevice;
use super::DmaPool;
use crate::log;
use crate::mm::KernelSlice;
use crate::sync::Lock;

use hid::HidDevice;

// xHCI Capability Register offsets (from BAR0)
const CAP_CAPLENGTH:  u64 = 0x00; // u8
const CAP_HCSPARAMS1: u64 = 0x04; // u32
const CAP_HCSPARAMS2: u64 = 0x08; // u32
const CAP_HCCPARAMS1: u64 = 0x10; // u32
const CAP_DBOFF:      u64 = 0x14; // u32
const CAP_RTSOFF:     u64 = 0x18; // u32

// xHCI Operational Register offsets (from op_base = BAR0 + cap_length)
const OP_USBCMD:   u64 = 0x00;
const OP_USBSTS:   u64 = 0x04;
const OP_PAGESIZE: u64 = 0x08;
const OP_CRCR:     u64 = 0x18; // 64-bit
const OP_DCBAAP:   u64 = 0x30; // 64-bit
const OP_CONFIG:   u64 = 0x38;
const OP_PORT_BASE: u64 = 0x400;
const PORT_REG_SIZE: u64 = 0x10;

// PORTSC bits
const PORTSC_CCS: u32 = 1 << 0;
const PORTSC_PED: u32 = 1 << 1;
const PORTSC_PR:  u32 = 1 << 4;
const PORTSC_CSC: u32 = 1 << 17;
const PORTSC_PRC: u32 = 1 << 21;
// All write-1-to-clear bits in PORTSC (must be masked during read-modify-write)
const PORTSC_RW1C: u32 = PORTSC_CSC | (1 << 18) | (1 << 19) | (1 << 20)
    | PORTSC_PRC | (1 << 22) | (1 << 23);

// Runtime Register offsets (from rt_base = BAR0 + rts_offset)
// Interrupter 0 starts at offset 0x20
const IR0_IMAN:   u64 = 0x20; // Interrupt Management (IP + IE)
const IR0_IMOD:   u64 = 0x24; // Interrupt Moderation
const IR0_ERSTSZ: u64 = 0x28;
const IR0_ERSTBA: u64 = 0x30; // 64-bit
const IR0_ERDP:   u64 = 0x38; // 64-bit

// xHCI interrupt vector
const XHCI_VECTOR: u8 = 0x21;

// TRB (Transfer Request Block) — 16 bytes
#[repr(C)]
#[derive(Clone, Copy)]
struct Trb {
    param: u64,
    status: u32,
    control: u32,
}

impl Trb {
    const ZERO: Self = Self { param: 0, status: 0, control: 0 };
}

const TRB_CYCLE: u32 = 1;

// TRB type field is bits [15:10]
const fn trb_type(t: u32) -> u32 { t << 10 }

const TRB_NORMAL:       u32 = trb_type(1);
const TRB_SETUP_STAGE:  u32 = trb_type(2);
const TRB_DATA_STAGE:   u32 = trb_type(3);
const TRB_STATUS_STAGE: u32 = trb_type(4);
const TRB_LINK:         u32 = trb_type(6);

const TRB_ENABLE_SLOT:    u32 = trb_type(9);
const TRB_ADDRESS_DEVICE: u32 = trb_type(11);
const TRB_CONFIGURE_EP:   u32 = trb_type(12);
const TRB_RESET_ENDPOINT: u32 = trb_type(14);
const TRB_SET_TR_DEQUEUE: u32 = trb_type(16);

// Event TRB types (read from event ring, encoded in bits [15:10])
const EVENT_TRANSFER:     u32 = 32;
const EVENT_CMD_COMPLETE: u32 = 33;

// Completion codes worth naming. A transfer that moved less than it asked for
// reports Short Packet, which is a success with a residue and not an error —
// reading it as one is the classic mass-storage bug, since every SCSI command
// that under-delivers takes that path.
const CC_SUCCESS: u32 = 1;
const CC_STALL: u32 = 6;
const CC_SHORT_PACKET: u32 = 13;

/// How long the driver waits on any one command or transfer.
///
/// A device that never answers must cost that device and not the CPU that
/// asked it — which is the whole reason this exists, because every wait in
/// this driver used to be an unbounded `spin_loop`. The bound is generous on
/// purpose: the transfers it covers complete in microseconds even under TCG,
/// so nothing but a dead device can reach it.
const USB_TIMEOUT_NS: u64 = 2_000_000_000;

/// When a wait started now would give up. Before `clock::init` this is 0 plus
/// the timeout and `nanos_since_boot` stays 0, so the wait is unbounded — the
/// behaviour this driver had everywhere, and reachable only by a caller that
/// runs before phase 2.
fn deadline() -> u64 {
    crate::clock::nanos_since_boot() + USB_TIMEOUT_NS
}

/// Spin until `ready`, and say whether that happened inside [`USB_TIMEOUT_NS`].
///
/// The register bits this covers are ones the controller sets in microseconds;
/// one that never sets belongs to a controller or a port this driver cannot
/// drive, and every caller turns `false` into a refusal that names it. Before
/// this existed the five of them were bare `spin_loop`s, which on a machine
/// with no serial port is the same picture as every other way a boot can stop:
/// `Boot: peripherals ready` painted on the panel, forever.
fn settles(ready: impl Fn() -> bool) -> bool {
    let deadline = deadline();
    while !ready() {
        if crate::clock::nanos_since_boot() >= deadline {
            return false;
        }
        core::hint::spin_loop();
    }
    true
}

/// Let a test starve one of those waits on a controller that is otherwise
/// answering perfectly.
///
/// Kernel features because nothing on the host side can stage them: QEMU's xHC
/// halts, resets, clears CNR and starts in microseconds, and its ports finish a
/// reset synchronously — there is no device or machine property that makes a
/// register bit not settle, and unplugging between the scan and the reset is
/// not expressible either. The rest of bring-up runs unchanged, so what these
/// certify is the deadline and the refusal, which is exactly the code that has
/// no other way to execute. Same reason `xhci-one-slot` and `i8042-fault`
/// exist.
const CONTROLLER_ANSWERS: bool = !cfg!(feature = "xhci-deaf-controller");
const PORT_ANSWERS: bool = !cfg!(feature = "xhci-deaf-port");

const RING_SIZE: usize = 256; // TRBs per ring (one page = 256 * 16)

/// Event Ring Segment Table entry (16 bytes).
#[repr(C)]
struct ErstEntry {
    ring_base: u64,
    ring_size: u32,
    _reserved: u32,
}

#[derive(Clone, Copy)]
struct TrbRing {
    base: *mut Trb,
    base_phys: u64,
    tail: u16,
    cycle: bool,
}

impl TrbRing {
    /// A ring the controller has never seen: zeroed, with the wrap link TRB
    /// already at the last slot and the enqueue pointer at the first.
    ///
    /// Also the recovery primitive. After a stall the controller's dequeue
    /// pointer is somewhere in the middle of a ring holding TRBs it will never
    /// run, so recovery is this plus a Set TR Dequeue Pointer naming
    /// [`Self::dequeue`] — the two have to agree or the endpoint resumes on
    /// stale TRBs.
    fn init(buf: KernelSlice) -> Self {
        assert!(buf.size() >= RING_SIZE * core::mem::size_of::<Trb>());
        unsafe { buf.zero(); }
        let mut link = Trb::ZERO;
        link.param = buf.phys();
        link.control = TRB_LINK | (1 << 1); // TC (Toggle Cycle)
        unsafe { write_volatile((buf.base() as *mut Trb).add(RING_SIZE - 1), link); }
        Self { base: buf.base() as *mut Trb, base_phys: buf.phys(), tail: 0, cycle: true }
    }

    fn new(buf: KernelSlice) -> Self {
        Self { base: buf.base() as *mut Trb, base_phys: buf.phys(), tail: 0, cycle: true }
    }

    /// Where the controller should resume, with the cycle state it must expect.
    fn dequeue(&self) -> u64 {
        self.base_phys + (self.tail as u64) * 16 | (self.cycle as u64)
    }

    fn enqueue(&mut self, mut trb: Trb) {
        if self.cycle {
            trb.control |= TRB_CYCLE;
        } else {
            trb.control &= !TRB_CYCLE;
        }
        unsafe { write_volatile(self.base.add(self.tail as usize), trb); }
        self.tail += 1;

        if self.tail as usize >= RING_SIZE - 1 {
            let mut link = Trb::ZERO;
            link.param = self.base_phys;
            link.control = TRB_LINK | (1 << 1); // TC (Toggle Cycle)
            if self.cycle { link.control |= TRB_CYCLE; }
            unsafe { write_volatile(self.base.add(self.tail as usize), link); }
            self.tail = 0;
            self.cycle = !self.cycle;
        }
    }
}

/// The granularity every structure below is placed at. It is the xHCI
/// PAGESIZE the controller reports, and no shipping xHC reports anything else;
/// `init` asserts the register rather than trusting the coincidence.
const PAGE: usize = 0x1000;

// The pool's fixed head. Everything here is either the controller's own state
// or enumeration scratch, and there is exactly one of each because enumeration
// is serial — see `device::init_device`.
const OFF_DCBAA: usize     = 0 * PAGE; // (max_slots + 1) * 8, 2 KiB at most
const OFF_CMD_RING: usize  = 1 * PAGE;
const OFF_ERST: usize      = 2 * PAGE;
const OFF_EVT_RING: usize  = 3 * PAGE;
const OFF_INPUT_CTX: usize = 4 * PAGE; // 33 contexts, so 2112 B at ctx_size 64
const OFF_DATA_BUF: usize  = 5 * PAGE;
const SHARED_SIZE: usize   = 6 * PAGE;

// One of these per device the controller gives us a slot for. All four
// outlive enumeration: the controller writes the output context and the report
// buffer for as long as the device is attached, and the interrupt ring carries
// that device's transfers. Sharing any of them between two devices is a
// silent data race, which is what keying the interrupt ring by HID class did.
//
// The EP0 ring is here for the same reason and used to be one shared page.
// That was sound only while every control transfer happened during that
// device's own enumeration: the ring is rewound for each device, so a device
// enumerated earlier has an EP0 dequeue pointer into a ring whose contents and
// cycle state have since moved under it. Mass storage is the first thing that
// needs to talk to a device *after* boot — Clear-Feature(HALT) and Bulk-Only
// Reset are control transfers on the recovery path — so the ring has to belong
// to the device rather than to the enumeration.
const DEV_INT_RING: usize = 0;                 // 256 TRBs, exactly one page
const DEV_EP0_RING: usize = PAGE;              // likewise
const DEV_OUT_CTX: usize  = 2 * PAGE;          // 32 contexts, 2 KiB at ctx_size 64
const DEV_REPORT: usize   = 2 * PAGE + 0x800;  // 8 B, the largest boot report
const DEV_STRIDE: usize   = 3 * PAGE;

// One of these per mass-storage device, and separate from the device block
// above because the two are three orders of magnitude apart in appetite: a
// keyboard needs 8 bytes of report buffer and a disk needs a transfer buffer.
// Folding the larger into `DEV_STRIDE` would hand every keyboard, hub and
// camera on the bus a 64 KiB block it never touches, and would divide the
// number of devices the pool can track by eight.
const MSC_IN_RING: usize   = 0;
const MSC_OUT_RING: usize  = PAGE;
const MSC_CBW: usize       = 2 * PAGE;         // 31 B
const MSC_CSW: usize       = 2 * PAGE + 0x40;  // 13 B
const MSC_SCRATCH: usize   = 2 * PAGE + 0x80;  // INQUIRY, READ CAPACITY, sense
const MSC_SCRATCH_LEN: usize = 64;
/// The bulk data buffer, placed so it cannot cross a 64 KiB boundary — which
/// is the one placement rule an xHCI Normal TRB's buffer has. `msc_base` is
/// aligned to `MSC_STRIDE` and the pool's physical base is 2 MiB aligned, so
/// every block starts on a 64 KiB boundary and this buffer occupies its
/// second half exactly.
const MSC_DATA: usize      = 8 * PAGE;
const MSC_DATA_LEN: usize  = 8 * PAGE;
const MSC_STRIDE: usize    = 16 * PAGE;

/// Mass-storage devices the pool has blocks for. Two, because that is what a
/// machine booting off a USB stick with a second one plugged in has, and each
/// costs 64 KiB whether or not it is used. A third stick is refused by name
/// rather than served from somebody else's block.
const MSC_BLOCKS: usize = 2;

/// The largest run of 4 KiB blocks one SCSI command moves, which is the data
/// buffer over the trait's block size. Every caller-facing loop batches to it.
const MSC_MAX_BLOCKS: u32 = (MSC_DATA_LEN / 4096) as u32;

/// Device blocks to size the pool for before the controller's slot count is
/// consulted.
///
/// A scratchpad demand that lands `dev_base` on or just under a 2 MiB boundary
/// leaves little or nothing in the page it forced us to allocate anyway, and
/// then MaxSlotsEn is written as that number and the controller enumerates
/// nothing. This is not defensive padding; it is what keeps the pool from
/// having no room for devices.
///
/// Swept over all 1024 demands HCSPARAMS2's two 5-bit fields can express:
/// without this floor `dev_blocks` is **0** for 32 of them (458–473 and
/// 969–984) and as few as 5 for another 32 (442–457, 953–968); with it the
/// smallest is 10, at 426.
const MIN_DEVICE_BLOCKS: usize = 8;

/// Cap the driver at one device block, so a test can drive the path where the
/// controller hands back a slot the pool has no room for. Nothing else can
/// stage it: QEMU's `nec-usb-xhci,slots=N` does not reach HCSPARAMS1 and its
/// Enable Slot ignores MaxSlotsEn, and a real pool holds ~250 devices.
#[cfg(feature = "xhci-one-slot")]
const DEVICE_CEILING: usize = 1;
#[cfg(not(feature = "xhci-one-slot"))]
const DEVICE_CEILING: usize = usize::MAX;

/// Where each structure sits in the pool, derived from what the controller
/// reported. Nothing here is a constant except the strides above.
#[derive(Clone, Copy)]
struct Layout {
    scratch_array: usize,
    scratch_buffers: usize,
    scratch_count: usize,
    msc_base: usize,
    dev_base: usize,
    /// Device blocks the pool holds, which is also the MaxSlotsEn written to
    /// CONFIG: the controller is told exactly what the driver can track.
    dev_blocks: usize,
    pool_size: usize,
}

impl Layout {
    /// `max_scratchpad` and `max_slots` come straight off HCSPARAMS, which is
    /// where every number below stops being arbitrary.
    ///
    /// It is also what makes the plain `align_2m` below safe where every other
    /// caller taking a size from outside the kernel needs `align_2m_checked`:
    /// `max_scratchpad` is two 5-bit HCSPARAMS2 fields (`init` masks both with
    /// `0x1F`), so it is at most 1023, and the mass-storage array adds a fixed
    /// 128 KiB on top — `dev_base` is at most 4,390,912 B, or 4.19 MiB, swept
    /// over every demand. A controller cannot report a number that overflows
    /// this, whatever it says.
    fn new(max_scratchpad: usize, max_slots: u8) -> Self {
        let scratch_array = SHARED_SIZE;
        let array_bytes = (max_scratchpad * 8 + PAGE - 1) & !(PAGE - 1);
        let scratch_buffers = scratch_array + array_bytes;
        // Ahead of the device array rather than behind it, so the device array
        // still absorbs all of the pool's slack and MaxSlotsEn is unchanged by
        // storage existing. The alignment is what makes each block's data
        // buffer stay inside one 64 KiB region.
        let msc_base =
            (scratch_buffers + max_scratchpad * PAGE + MSC_STRIDE - 1) & !(MSC_STRIDE - 1);
        let dev_base = msc_base + MSC_BLOCKS * MSC_STRIDE;

        // DmaPool hands out whole 2 MiB pages. The head above already forces
        // one, so every block that fits in its slack is free — and asking for
        // more than the slack buys a second 2 MiB page for devices no root hub
        // has ports for. The floor is what decides how many pages to take; the
        // slack of those pages is what decides how many blocks to carve.
        let pool_size = crate::mm::align_2m(dev_base + MIN_DEVICE_BLOCKS * DEV_STRIDE);
        let dev_blocks = ((pool_size - dev_base) / DEV_STRIDE)
            .min(max_slots as usize)
            .min(DEVICE_CEILING);

        Self {
            scratch_array,
            scratch_buffers,
            scratch_count: max_scratchpad,
            msc_base,
            dev_base,
            dev_blocks,
            pool_size,
        }
    }

    /// The block belonging to a 1-based slot id, or `None` when the controller
    /// handed back a slot the pool has no room for.
    fn device(&self, slot_id: u8) -> Option<usize> {
        let index = (slot_id as usize).checked_sub(1)?;
        (index < self.dev_blocks).then(|| self.dev_base + index * DEV_STRIDE)
    }

    /// The block for the `index`-th mass-storage device bound this boot.
    fn msc(&self, index: usize) -> Option<usize> {
        (index < MSC_BLOCKS).then(|| self.msc_base + index * MSC_STRIDE)
    }
}

fn setup_packet(bm_request_type: u8, b_request: u8, w_value: u16, w_index: u16, w_length: u16) -> u64 {
    (bm_request_type as u64)
        | ((b_request as u64) << 8)
        | ((w_value as u64) << 16)
        | ((w_index as u64) << 32)
        | ((w_length as u64) << 48)
}

// SAFETY: XhciController contains raw pointers to DMA memory that is valid
// for the lifetime of the controller. Access is serialized by the Lock.
unsafe impl Send for XhciController {}

pub struct XhciController {
    db_base: Mmio,
    rt_base: Mmio,

    context_size: usize, // 32 or 64
    layout: Layout,

    /// This controller's DMA, and this controller's only. It used to be one
    /// static for the driver, which was sound only while the machine had one
    /// controller: every offset in `Layout` is relative to a pool base, so two
    /// controllers sharing one pool put both their DCBAAs, both their command
    /// rings and both their slot 1 device contexts at the same address.
    pool: DmaPool,

    /// The machine-wide number of this controller's first disk. Blocks in the
    /// pool are indexed per controller and everything above `storage_read` is
    /// indexed per machine, so this is what keeps a log line from calling two
    /// different disks "disk 0".
    disk_base: usize,

    cmd_ring: TrbRing,

    event_ring: *const Trb,
    event_head: u16,
    event_phase: bool,

    devices: Vec<HidDevice>,
    storage: Vec<msc::MscDevice>,
}

impl XhciController {
    pub(super) fn dma(&self) -> KernelSlice {
        self.pool.slice()
    }

    fn submit_command(&mut self, trb: Trb) {
        self.cmd_ring.enqueue(trb);
        fence(Ordering::Release);
        self.db_base.write_u32(0, 0);
    }

    /// One event, or `None` while the controller has not published the next.
    /// Every reader goes through here, because the ring is a single queue
    /// carrying command completions, the enumeration's own control transfers
    /// and every bound device's interrupt completions at once — so a reader
    /// that dequeues an event it did not ask for owes it to whoever did, which
    /// is what `dispatch_event` is.
    fn next_event(&mut self) -> Option<Trb> {
        let event = unsafe { read_volatile(self.event_ring.add(self.event_head as usize)) };
        if ((event.control & 1) != 0) != self.event_phase {
            return None;
        }
        self.advance_event_ring();
        Some(event)
    }

    /// Give an event to the device it names. A bound device's interrupt
    /// endpoint carries exactly one queued TRB and `requeue` is the only thing
    /// that puts the next one there, so an interrupt completion dropped here —
    /// as one dequeued during a later port's enumeration used to be — leaves
    /// that device with an empty ring for the life of the boot: no log line, no
    /// fault, a keyboard that simply stops.
    fn dispatch_event(&mut self, event: Trb) {
        let trb_type = (event.control >> 10) & 0x3F;
        let code = (event.status >> 24) & 0xFF;
        let slot = ((event.control >> 24) & 0xFF) as u8;
        if trb_type != EVENT_TRANSFER || (code != 1 && code != 13) {
            return;
        }
        if let Some(dev) = self.devices.iter_mut().find(|d| d.slot_id == slot) {
            dev.dispatch_report();
            dev.requeue(&self.db_base);
        }
    }

    /// The completion code and slot id of the command just submitted, or
    /// `None` if the controller never answered.
    fn wait_command(&mut self) -> Option<(u32, u32)> {
        let deadline = deadline();
        loop {
            let Some(event) = self.next_event() else {
                if crate::clock::nanos_since_boot() >= deadline {
                    return None;
                }
                core::hint::spin_loop();
                continue;
            };
            if (event.control >> 10) & 0x3F == EVENT_CMD_COMPLETE {
                return Some(((event.status >> 24) & 0xFF, (event.control >> 24) & 0xFF));
            }
            self.dispatch_event(event);
        }
    }

    /// Submit `trb` and report the completion code, logging and returning
    /// `None` on anything the controller did not accept. `what` names the
    /// command in that line, because a bare code is unreadable at 3am.
    fn run_command(&mut self, trb: Trb, what: &str) -> Option<u32> {
        self.submit_command(trb);
        match self.wait_command() {
            Some((CC_SUCCESS, _)) => Some(CC_SUCCESS),
            Some((code, _)) => {
                log!("xHCI: {what} failed, code={code}");
                None
            }
            None => {
                log!("xHCI: {what} timed out");
                None
            }
        }
    }

    fn advance_event_ring(&mut self) {
        self.event_head = (self.event_head + 1) % RING_SIZE as u16;
        if self.event_head == 0 {
            self.event_phase = !self.event_phase;
        }
        let erdp = self.dma().phys() + OFF_EVT_RING as u64 + (self.event_head as u64) * 16;
        self.rt_base.write_u64(IR0_ERDP, erdp | (1 << 3)); // EHB clears interrupt pending
        self.rt_base.write_u32(IR0_IMAN, 3); // clear IP (W1C) + keep IE
    }

    fn ring_doorbell(&self, slot: u8, dci: u8) {
        fence(Ordering::Release);
        self.db_base.write_u32(slot as u64 * 4, dci as u32);
    }

    /// The completion of the transfer just queued on (`slot`, `dci`), as a
    /// completion code and the number of bytes the controller did *not* move.
    ///
    /// The event ring is one queue for the whole controller, so anything that
    /// arrives here and is not ours belongs to a bound device delivering a
    /// report — handing it to `dispatch_event` rather than dropping it is what
    /// keeps that device's interrupt ring fed. Matching on the endpoint as
    /// well as the slot matters for mass storage, where one slot carries three
    /// endpoints and a stalled one still completes.
    fn wait_transfer(&mut self, slot: u8, dci: u8) -> Option<(u32, u32)> {
        let deadline = deadline();
        loop {
            let Some(event) = self.next_event() else {
                if crate::clock::nanos_since_boot() >= deadline {
                    return None;
                }
                core::hint::spin_loop();
                continue;
            };
            let trb_type = (event.control >> 10) & 0x3F;
            let ev_slot = ((event.control >> 24) & 0xFF) as u8;
            let ev_dci = ((event.control >> 16) & 0x1F) as u8;
            if trb_type == EVENT_TRANSFER && ev_slot == slot && ev_dci == dci {
                return Some(((event.status >> 24) & 0xFF, event.status & 0x00FF_FFFF));
            }
            self.dispatch_event(event);
        }
    }

    /// One control transfer on `ring`, which must be the EP0 ring named by
    /// `slot`'s device context. Returns the completion code, or `None` when
    /// the device never answered.
    fn control_transfer(
        &mut self,
        slot: u8,
        ring: &mut TrbRing,
        bm_request_type: u8,
        b_request: u8,
        w_value: u16,
        w_index: u16,
        data_buf: Option<u64>,
        data_len: u16,
    ) -> Option<u32> {
        let is_in = (bm_request_type & 0x80) != 0;
        let has_data = data_len > 0 && data_buf.is_some();
        let trt = if !has_data { 0u32 } else if is_in { 3 } else { 2 };

        let mut setup = Trb::ZERO;
        setup.param = setup_packet(bm_request_type, b_request, w_value, w_index, data_len);
        setup.status = 8;
        setup.control = TRB_SETUP_STAGE | (1 << 6) | (trt << 16);
        ring.enqueue(setup);

        if has_data {
            let mut data = Trb::ZERO;
            data.param = data_buf.unwrap();
            data.status = data_len as u32;
            let dir = if is_in { 1u32 << 16 } else { 0 };
            data.control = TRB_DATA_STAGE | dir;
            ring.enqueue(data);
        }

        let mut status = Trb::ZERO;
        let status_dir = if has_data && is_in { 0 } else { 1u32 << 16 };
        status.control = TRB_STATUS_STAGE | (1 << 5) | status_dir;
        ring.enqueue(status);

        self.ring_doorbell(slot, 1);
        self.wait_transfer(slot, 1).map(|(code, _)| code)
    }

    pub fn poll(&mut self) {
        while let Some(event) = self.next_event() {
            self.dispatch_event(event);
        }
    }

    fn write_ctx32(&self, ctx_base: *mut u8, slot_index: usize, dword: usize, val: u32) {
        let offset = (slot_index * self.context_size) + (dword * 4);
        unsafe { write_volatile(ctx_base.add(offset) as *mut u32, val); }
    }
}

/// Every xHCI controller on the machine, in PCI enumeration order.
///
/// A `Vec` and not an `Option`, because the machine this project targets has
/// two: Tiger Lake carries a USB4 xHCI in the Thunderbolt block *ahead* of the
/// PCH's on the bus, and the laptop's own ports hang off the second one. The
/// driver that kept one controller reported that the T14 had no USB input.
static XHCI: Lock<Vec<XhciController>> = Lock::new(Vec::new());

/// Process xHCI events only if this CPU has an unserviced interrupt record.
/// Records live on the CPU that took the interrupt (which its ISR forces
/// into the scheduler via need_resched), so on every other CPU this is one
/// uncontended atomic op on its own cache line — callers need no cpu gate.
///
/// Every controller is polled, because every controller's message carries the
/// same vector and `irq_ring` keeps one record per source: the record says
/// that *an* xHC interrupted, never which. Polling a quiet controller costs one
/// read of its event ring's next TRB.
///
/// Thread context only. It takes `XHCI` and dispatches HID reports, which take
/// the keyboard held-set and both event queues; an ISR calling this would spin
/// on whichever of those the thread it interrupted holds.
pub fn poll_if_pending() {
    if crate::irq_ring::take(crate::irq_ring::IrqSource::Xhci).is_some() {
        for ctrl in XHCI.lock().iter_mut() {
            ctrl.poll();
        }
    }
}

/// Everything above the controller needs to know about one bound disk.
#[derive(Clone, Copy, Debug)]
pub struct StorageGeometry {
    /// What the device itself addresses in, straight off READ CAPACITY.
    pub logical_block_bytes: u32,
    /// The same capacity in the 4 KiB blocks `BlockDevice` is written in.
    pub blocks: u64,
}

pub fn storage_count() -> usize {
    XHCI.lock().iter().map(|c| c.storage.len()).sum()
}

/// Run `f` against the machine's `index`-th disk.
///
/// Disks are numbered across the whole machine, in controller order: the index
/// the block layer holds names a device, and which controller bound it is not
/// something anything above here should have to know. A per-controller index
/// would make disk 0 mean two different disks on a two-controller machine.
fn with_disk<R>(index: usize, f: impl FnOnce(&mut XhciController, usize) -> R) -> Option<R> {
    let mut guard = XHCI.lock();
    let mut first = 0;
    for ctrl in guard.iter_mut() {
        let count = ctrl.storage.len();
        if index < first + count {
            return Some(f(ctrl, index - first));
        }
        first += count;
    }
    None
}

pub fn storage_geometry(index: usize) -> Option<StorageGeometry> {
    with_disk(index, |ctrl, local| ctrl.storage[local].geometry())
}

/// Whether the machine's `index`-th disk is still being spoken to.
pub fn storage_online(index: usize) -> Option<bool> {
    with_disk(index, |ctrl, local| ctrl.storage[local].online())
}

/// Read `count` 4 KiB blocks at `lba`. `false` means the transfer failed and
/// `buf` holds nothing the caller may believe.
pub fn storage_read(index: usize, lba: u64, count: u32, buf: &mut [u8]) -> bool {
    with_disk(index, |ctrl, local| ctrl.msc_read(local, lba, count, buf)).unwrap_or(false)
}

pub fn storage_write(index: usize, lba: u64, count: u32, buf: &[u8]) -> bool {
    with_disk(index, |ctrl, local| ctrl.msc_write(local, lba, count, buf)).unwrap_or(false)
}

pub fn storage_flush(index: usize) -> bool {
    with_disk(index, |ctrl, local| ctrl.msc_flush(local)).unwrap_or(false)
}

/// Point this controller's interrupts at [`XHCI_VECTOR`] and name the
/// mechanism that took them, or `None` when the function offers neither.
///
/// `None` has to be a refusal and not a degradation, and that is the whole
/// shape of this function. Every read of an event ring in this driver is
/// `poll_if_pending`, which runs only behind an `irq_ring` record that
/// nothing but vector 0x21's ISR publishes — so a controller whose messages
/// cannot reach a CPU is one whose ring is never read again. This used to log
/// "no MSI-X capability, using polled mode" and carry on: there is no polled
/// mode, and every device on such a controller enumerated, logged itself
/// ready, and delivered nothing for the life of the boot.
fn arm_interrupt(pci_dev: &PciDevice) -> Option<&'static str> {
    if pci_dev.enable_msix(XHCI_VECTOR) {
        return Some("MSI-X");
    }
    pci_dev.enable_msi(XHCI_VECTOR).then_some("MSI")
}

/// Bring up every xHCI controller on the machine.
///
/// Every one, not the first: a Tiger Lake laptop has two — the Thunderbolt
/// block's at 00:0d.0 and the PCH's at 00:14.0, identical in class, subclass
/// and prog_if — and its keyboard and USB-A ports are on the second. Taking
/// the first match reported that the T14 had no USB HID at all, which was true
/// of that controller and false of the machine.
pub fn init(devices: &[PciDevice]) {
    // Once for the machine, not once per controller: it reads no register and
    // touches no device, so a second run would say the same thing twice.
    #[cfg(feature = "xhci-descriptor-selftest")]
    device::selftest();

    let mut controllers = Vec::new();
    let mut disks = 0;
    let mut present = 0;
    for pci_dev in devices.iter().filter(|d| d.matches_class(0x0C, 0x03, Some(0x30))) {
        present += 1;
        if let Some(ctrl) = init_one(pci_dev, disks) {
            disks += ctrl.storage.len();
            controllers.push(ctrl);
        }
    }

    if controllers.is_empty() {
        // A machine with no xHC and a machine whose xHCs this driver refused
        // are different machines, and the second used to print the first's
        // line. The per-controller refusal above says why; this says that
        // nothing was left.
        match present {
            0 => log!("xHCI: no controller on this machine, USB input unavailable"),
            n => log!("xHCI: {n} controller(s) present, none of them usable, USB unavailable"),
        }
        return;
    }
    let hid: usize = controllers.iter().map(|c| c.devices.len()).sum();
    log!("xHCI: {} controller(s), {} HID device(s)", controllers.len(), hid);
    log!("usb-storage: {} device(s)", disks);
    *XHCI.lock() = controllers;
}

fn init_one(pci_dev: &PciDevice, disk_base: usize) -> Option<XhciController> {
    log!("xHCI: found at PCI {:02x}:{:02x}.{}", pci_dev.bus, pci_dev.dev, pci_dev.func);

    let bar_addr = pci_dev.read_bar_64(0);
    pci_dev.enable_bus_master();
    log!("xHCI: BAR0={:#x}", bar_addr);

    // Ahead of the reset and ahead of the port scan, because a controller
    // whose interrupts cannot be delivered must not reach either: the reset
    // is what makes it ours, and the port scan is what prints
    // `USB keyboard ready`. Refusing here leaves the controller exactly as
    // firmware left it, with nothing enumerated on it to claim otherwise.
    let Some(irq) = arm_interrupt(pci_dev) else {
        log!(
            "xHCI: NOT INITIALISED at PCI {:02x}:{:02x}.{} — the controller offers neither \
             MSI-X nor MSI, and this driver has no other way to be told it has anything to \
             say. No USB device on it can be used.",
            pci_dev.bus, pci_dev.dev, pci_dev.func
        );
        return None;
    };
    log!("xHCI: {irq} enabled (vector {XHCI_VECTOR:#x})");

    let bar = crate::mm::paging::kernel().lock().as_mut().unwrap().map_mmio(bar_addr, 0x10000);

    let cap_length = bar.read_u8(CAP_CAPLENGTH) as u64;
    let hcsparams1 = bar.read_u32(CAP_HCSPARAMS1);
    let hcsparams2 = bar.read_u32(CAP_HCSPARAMS2);
    let hccparams1 = bar.read_u32(CAP_HCCPARAMS1);
    let db_offset = (bar.read_u32(CAP_DBOFF) & !0x3) as u64;
    let rts_offset = (bar.read_u32(CAP_RTSOFF) & !0x1F) as u64;

    let max_slots = (hcsparams1 & 0xFF) as u8;
    let max_ports = ((hcsparams1 >> 24) & 0xFF) as u8;
    let csz = ((hccparams1 >> 2) & 1) != 0;
    let context_size: usize = if csz { 64 } else { 32 };

    // Everything below refuses this controller by name rather than taking the
    // machine with it. Two controllers is the target laptop's shape, and a
    // property of the empty Thunderbolt one is no reason the PCH's ports should
    // not come up.
    let refuse = |why: core::fmt::Arguments| {
        log!("xHCI: NOT INITIALISED at PCI {:02x}:{:02x}.{} — {why}. No USB device on it can \
             be used.", pci_dev.bus, pci_dev.dev, pci_dev.func);
    };

    // The BAR is mapped at a fixed 64 KiB and both offsets are the controller's
    // own 32-bit numbers, so this is where a controller that puts its doorbells
    // or its runtime registers outside the window has to be refused: the
    // subtraction below it underflows, and with overflow checks off it wraps
    // back to exactly `bar_size`, which `Mmio::subregion`'s own assertion then
    // accepts — an `Mmio` based outside the mapping, faulting on the first
    // doorbell write.
    let bar_size = 0x10000u64;
    let (Some(db_len), Some(rt_len)) =
        (bar_size.checked_sub(db_offset), bar_size.checked_sub(rts_offset))
    else {
        refuse(format_args!(
            "DBOFF={db_offset:#x} RTSOFF={rts_offset:#x} put its registers outside the \
             {bar_size:#x} window this driver maps"
        ));
        return None;
    };
    let op_base = bar.subregion(cap_length, bar_size - cap_length);
    let db_base = bar.subregion(db_offset, db_len);
    let rt_base = bar.subregion(rts_offset, rt_len);

    let pagesize = op_base.read_u32(OP_PAGESIZE) & 0xFFFF;
    log!("xHCI: max_slots={} max_ports={} ctx_size={} pagesize={:#x}",
        max_slots, max_ports, context_size, pagesize);
    // Bit 0 is 4 KiB, and it is the only bit this driver can use — the register
    // is a mask of the page sizes the controller supports, so the test is that
    // the bit is set and not that it is alone. The scratchpad is the whole
    // exposure: its entries are one PAGE apart, so a controller placing them at
    // 8 KiB writes each buffer over the next and the last one past `dev_base`
    // into block 0's interrupt ring — memory corruption with no diagnostic.
    // Every other consequence runs the safe way, since a larger page size only
    // relaxes the rule that the DCBAA and the contexts must not cross one.
    if pagesize & 1 == 0 {
        refuse(format_args!(
            "PAGESIZE={pagesize:#x} does not include 4 KiB, and every ring, context and \
             scratchpad buffer here is placed at 4 KiB"
        ));
        return None;
    }

    let max_sp_hi = ((hcsparams2 >> 21) & 0x1F) as usize;
    let max_sp_lo = ((hcsparams2 >> 27) & 0x1F) as usize;
    let layout = Layout::new((max_sp_hi << 5) | max_sp_lo, max_slots);
    log!("xHCI: dma {} KiB: scratchpad={} device blocks={} of {} B (max_slots={})",
        layout.pool_size / 1024, layout.scratch_count, layout.dev_blocks, DEV_STRIDE, max_slots);

    // Before the controller is touched at all: on a PC the firmware may still
    // own it for legacy keyboard emulation, and resetting a controller SMM is
    // driving is a fight with no diagnostic.
    legacy::take_ownership(&bar, bar_size, hccparams1);

    let usbcmd = op_base.read_u32(OP_USBCMD);
    if usbcmd & 1 != 0 {
        op_base.write_u32(OP_USBCMD, usbcmd & !1);
    }
    let deadline_ms = USB_TIMEOUT_NS / 1_000_000;
    if !settles(|| CONTROLLER_ANSWERS && op_base.read_u32(OP_USBSTS) & 1 != 0) {
        refuse(format_args!("it never halted, within {deadline_ms} ms of being asked to"));
        return None;
    }

    op_base.write_u32(OP_USBCMD, 1 << 1);
    if !settles(|| CONTROLLER_ANSWERS && op_base.read_u32(OP_USBCMD) & (1 << 1) == 0) {
        refuse(format_args!("it held HCRST for {deadline_ms} ms"));
        return None;
    }
    if !settles(|| CONTROLLER_ANSWERS && op_base.read_u32(OP_USBSTS) & (1 << 11) == 0) {
        refuse(format_args!("it stayed Controller Not Ready for {deadline_ms} ms after its reset"));
        return None;
    }
    log!("xHCI: controller reset");

    // After the reset, so a controller refused above costs no physical memory
    // at all — and the pool is freed with the `DmaPool` on every refusal below,
    // since `PhysPage` gives its page back when dropped.
    let pool = DmaPool::alloc(layout.pool_size);

    // MaxSlotsEn is what the driver can track, not what the controller can
    // offer: a conformant xHC then refuses Enable Slot past it rather than
    // handing back an id with nowhere to put its context.
    op_base.write_u32(OP_CONFIG, layout.dev_blocks as u32);

    let dma = pool.slice();
    unsafe { dma.zero(); }

    if layout.scratch_count > 0 {
        let array = dma.ptr_at(layout.scratch_array) as *mut u64;
        for i in 0..layout.scratch_count {
            let buf = dma.phys() + (layout.scratch_buffers + i * PAGE) as u64;
            unsafe { write_volatile(array.add(i), buf); }
        }
        unsafe {
            write_volatile(
                dma.ptr_at(OFF_DCBAA) as *mut u64,
                dma.phys() + layout.scratch_array as u64,
            );
        }
        log!("xHCI: {} scratchpad buffers configured", layout.scratch_count);
    }

    op_base.write_u64(OP_DCBAAP, dma.phys() + OFF_DCBAA as u64);

    let cmd_ring = TrbRing::init(dma.subslice(OFF_CMD_RING, PAGE));
    op_base.write_u64(OP_CRCR, dma.phys() + OFF_CMD_RING as u64 | 1);

    let evt_ring_buf = dma.subslice(OFF_EVT_RING, PAGE);
    let erst = dma.ptr_at(OFF_ERST) as *mut ErstEntry;
    unsafe {
        write_volatile(erst, ErstEntry {
            ring_base: evt_ring_buf.phys(),
            ring_size: RING_SIZE as u32,
            _reserved: 0,
        });
    }
    rt_base.write_u32(IR0_ERSTSZ, 1);
    rt_base.write_u64(IR0_ERDP, evt_ring_buf.phys());
    rt_base.write_u64(IR0_ERSTBA, dma.phys() + OFF_ERST as u64);

    // Enable interrupter 0
    rt_base.write_u32(IR0_IMOD, 0);
    rt_base.write_u32(IR0_IMAN, 3);

    // Start controller (R/S + INTE for interrupt delivery)
    op_base.write_u32(OP_USBCMD, 1 | (1 << 2));
    if !settles(|| CONTROLLER_ANSWERS && op_base.read_u32(OP_USBSTS) & 1 == 0) {
        refuse(format_args!("it stayed halted for {deadline_ms} ms after R/S"));
        return None;
    }
    log!("xHCI: controller started");

    let mut ctrl = XhciController {
        db_base,
        rt_base,
        context_size,
        layout,
        pool,
        disk_base,
        cmd_ring,
        event_ring: evt_ring_buf.base() as *const Trb,
        event_head: 0,
        event_phase: true,
        devices: Vec::new(),
        storage: Vec::new(),
    };

    device::scan_ports(&mut ctrl, &op_base, max_ports);

    // A controller with no HID on it is still a controller, and returning it
    // is not a formality: it has been reset, started and armed, so dropping it
    // here leaves a live interrupter with nothing draining its event ring. It
    // is also the ordinary state of the target laptop, whose
    // keyboard is PS/2 and whose touchpad is I2C-HID — under metal-sim this
    // `None` reached `kernel_main`'s `.expect` and panicked the boot.
    if ctrl.devices.is_empty() {
        log!("xHCI: no HID devices on the controller at {:02x}:{:02x}.{}",
            pci_dev.bus, pci_dev.dev, pci_dev.func);
    }

    Some(ctrl)
}
