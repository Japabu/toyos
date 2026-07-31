mod device;
mod hid;

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

// MSI-X PCI capability ID
const PCI_CAP_MSIX: u8 = 0x11;
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

// Event TRB types (read from event ring, encoded in bits [15:10])
const EVENT_TRANSFER:     u32 = 32;
const EVENT_CMD_COMPLETE: u32 = 33;

const RING_SIZE: usize = 256; // TRBs per ring (one page = 256 * 16)

/// Event Ring Segment Table entry (16 bytes).
#[repr(C)]
struct ErstEntry {
    ring_base: u64,
    ring_size: u32,
    _reserved: u32,
}

struct TrbRing {
    base: *mut Trb,
    base_phys: u64,
    tail: u16,
    cycle: bool,
}

impl TrbRing {
    fn new(buf: KernelSlice) -> Self {
        Self { base: buf.base() as *mut Trb, base_phys: buf.phys(), tail: 0, cycle: true }
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
/// `init` logs the register so a machine that disagrees says so.
const PAGE: usize = 0x1000;

// The pool's fixed head. Everything here is either the controller's own state
// or enumeration scratch, and there is exactly one of each because enumeration
// is serial — see `device::init_device`.
const OFF_DCBAA: usize     = 0 * PAGE; // (max_slots + 1) * 8, 2 KiB at most
const OFF_CMD_RING: usize  = 1 * PAGE;
const OFF_ERST: usize      = 2 * PAGE;
const OFF_EVT_RING: usize  = 3 * PAGE;
const OFF_INPUT_CTX: usize = 4 * PAGE; // 33 contexts, so 2112 B at ctx_size 64
const OFF_EP0_RING: usize  = 5 * PAGE;
const OFF_DATA_BUF: usize  = 6 * PAGE;
const SHARED_SIZE: usize   = 7 * PAGE;

// One of these per device the controller gives us a slot for. All three
// outlive enumeration: the controller writes the output context and the report
// buffer for as long as the device is attached, and the interrupt ring carries
// that device's transfers. Sharing any of them between two devices is a
// silent data race, which is what keying the interrupt ring by HID class did.
const DEV_INT_RING: usize = 0;             // 256 TRBs, exactly one page
const DEV_OUT_CTX: usize  = PAGE;          // 32 contexts, 2 KiB at ctx_size 64
const DEV_REPORT: usize   = PAGE + 0x800;  // 8 B, the largest boot report
const DEV_STRIDE: usize   = 2 * PAGE;

/// Device blocks to size the pool for before the controller's slot count is
/// consulted. Only a controller with a scratchpad demand that lands just under
/// a 2 MiB boundary can leave fewer than this in the page it forced us to
/// allocate anyway; without the floor such a controller would get a working
/// pool with room for one device.
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
    dev_base: usize,
    /// Device blocks the pool holds, which is also the MaxSlotsEn written to
    /// CONFIG: the controller is told exactly what the driver can track.
    dev_blocks: usize,
    pool_size: usize,
}

impl Layout {
    /// `max_scratchpad` and `max_slots` come straight off HCSPARAMS, which is
    /// where every number below stops being arbitrary.
    fn new(max_scratchpad: usize, max_slots: u8) -> Self {
        let scratch_array = SHARED_SIZE;
        let array_bytes = (max_scratchpad * 8 + PAGE - 1) & !(PAGE - 1);
        let scratch_buffers = scratch_array + array_bytes;
        let dev_base = scratch_buffers + max_scratchpad * PAGE;

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
}

static XHCI_DMA_POOL: Lock<Option<DmaPool>> = Lock::new(None);

fn dma() -> KernelSlice {
    XHCI_DMA_POOL.lock().as_ref().unwrap().slice()
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

    cmd_ring: TrbRing,
    ep0_ring: TrbRing,

    event_ring: *const Trb,
    event_head: u16,
    event_phase: bool,

    /// The device `init_device` is enumerating: which doorbell EP0 transfers
    /// ring, and which slot id their completions must carry to be this
    /// enumeration's rather than a bound device's.
    active_slot: u8,

    devices: Vec<HidDevice>,
}

impl XhciController {
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

    fn wait_command(&mut self) -> (u32, u32) {
        loop {
            let Some(event) = self.next_event() else {
                core::hint::spin_loop();
                continue;
            };
            if (event.control >> 10) & 0x3F == EVENT_CMD_COMPLETE {
                return ((event.status >> 24) & 0xFF, (event.control >> 24) & 0xFF);
            }
            self.dispatch_event(event);
        }
    }

    fn advance_event_ring(&mut self) {
        self.event_head = (self.event_head + 1) % RING_SIZE as u16;
        if self.event_head == 0 {
            self.event_phase = !self.event_phase;
        }
        let erdp = dma().phys() + OFF_EVT_RING as u64 + (self.event_head as u64) * 16;
        self.rt_base.write_u64(IR0_ERDP, erdp | (1 << 3)); // EHB clears interrupt pending
        self.rt_base.write_u32(IR0_IMAN, 3); // clear IP (W1C) + keep IE
    }

    fn enqueue_ep0(&mut self, trb: Trb) {
        self.ep0_ring.enqueue(trb);
    }

    fn ring_ep0_doorbell(&self) {
        fence(Ordering::Release);
        self.db_base.write_u32(self.active_slot as u64 * 4, 1);
    }

    /// The completion of the transfer just queued on the shared EP0 ring, which
    /// is the one carrying the enumerating device's slot id. Any other transfer
    /// event is a bound device delivering a report while this port enumerates;
    /// returning its completion code would have the caller read a descriptor
    /// buffer the controller has not written into yet.
    fn wait_transfer(&mut self) -> u32 {
        loop {
            let Some(event) = self.next_event() else {
                core::hint::spin_loop();
                continue;
            };
            let trb_type = (event.control >> 10) & 0x3F;
            let slot = ((event.control >> 24) & 0xFF) as u8;
            if trb_type == EVENT_TRANSFER && slot == self.active_slot {
                return (event.status >> 24) & 0xFF;
            }
            self.dispatch_event(event);
        }
    }

    fn control_transfer(
        &mut self,
        bm_request_type: u8,
        b_request: u8,
        w_value: u16,
        w_index: u16,
        data_buf: Option<u64>,
        data_len: u16,
    ) -> u32 {
        let is_in = (bm_request_type & 0x80) != 0;
        let has_data = data_len > 0 && data_buf.is_some();
        let trt = if !has_data { 0u32 } else if is_in { 3 } else { 2 };

        let mut setup = Trb::ZERO;
        setup.param = setup_packet(bm_request_type, b_request, w_value, w_index, data_len);
        setup.status = 8;
        setup.control = TRB_SETUP_STAGE | (1 << 6) | (trt << 16);
        self.enqueue_ep0(setup);

        if has_data {
            let mut data = Trb::ZERO;
            data.param = data_buf.unwrap();
            data.status = data_len as u32;
            let dir = if is_in { 1u32 << 16 } else { 0 };
            data.control = TRB_DATA_STAGE | dir;
            self.enqueue_ep0(data);
        }

        let mut status = Trb::ZERO;
        let status_dir = if has_data && is_in { 0 } else { 1u32 << 16 };
        status.control = TRB_STATUS_STAGE | (1 << 5) | status_dir;
        self.enqueue_ep0(status);

        self.ring_ep0_doorbell();
        self.wait_transfer()
    }

    /// Hand the one EP0 ring to the device being enumerated. Every device
    /// addressed before this one still points its EP0 context here, which is
    /// safe only because nothing rings their doorbells again: `init_device` is
    /// the sole caller, it runs once per port from `init`, and each of its
    /// control transfers completes before the next starts — the last of which
    /// holds only because `wait_transfer` matches the completion's slot id
    /// against the enumerating device, so a bound device delivering a report
    /// cannot end this transfer early and let the zeroing below land on a ring
    /// still in flight. Hotplug breaks the serial part and has to take an
    /// enumeration lock, not a second ring.
    fn reset_ep0_ring(&mut self) {
        let dma = dma();
        let ring = dma.subslice(OFF_EP0_RING, PAGE);
        unsafe { ring.zero(); }
        let mut link = Trb::ZERO;
        link.param = ring.phys();
        link.control = TRB_LINK | (1 << 1);
        unsafe { write_volatile((ring.base() as *mut Trb).add(RING_SIZE - 1), link); }
        self.ep0_ring = TrbRing::new(ring);
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

static XHCI: Lock<Option<XhciController>> = Lock::new(None);

pub fn set_global(ctrl: XhciController) {
    *XHCI.lock() = Some(ctrl);
}

/// Process xHCI events only if this CPU has an unserviced MSI-X record.
/// Records live on the CPU that took the interrupt (which its ISR forces
/// into the scheduler via need_resched), so on every other CPU this is one
/// uncontended atomic op on its own cache line — callers need no cpu gate.
///
/// Thread context only. It takes `XHCI` and dispatches HID reports, which take
/// the keyboard held-set and both event queues; an ISR calling this would spin
/// on whichever of those the thread it interrupted holds.
pub fn poll_if_pending() {
    if crate::irq_ring::take(crate::irq_ring::IrqSource::Xhci).is_some() {
        let mut guard = XHCI.lock();
        if let Some(ctrl) = guard.as_mut() {
            ctrl.poll();
        }
    }
}

fn setup_msix(pci_dev: &PciDevice) {
    let cap = pci_dev.capabilities().find(|c| c.id() == PCI_CAP_MSIX);
    let cap = match cap {
        Some(c) => c,
        None => {
            log!("xHCI: no MSI-X capability, using polled mode");
            return;
        }
    };

    let table_info = cap.read_u32(4);
    let table_bir = (table_info & 0x7) as u8;
    let table_offset = (table_info & !0x7) as u64;
    let table_bar = pci_dev.read_bar_64(table_bir);
    let table_addr = table_bar + table_offset;

    let table = crate::mm::paging::kernel().lock().as_mut().unwrap().map_mmio(table_addr, 0x1000);

    // Configure entry 0: route to LAPIC with vector XHCI_VECTOR
    table.write_u32(0x00, 0xFEE0_0000); // msg_addr_lo: LAPIC base
    table.write_u32(0x04, 0);            // msg_addr_hi
    table.write_u32(0x08, XHCI_VECTOR as u32); // msg_data: vector
    table.write_u32(0x0C, 0);            // vector control: unmask

    // Enable MSI-X in capability (bit 15), clear function mask (bit 14)
    let msg_ctrl = cap.read_u16(2);
    cap.write_u16(2, (msg_ctrl | (1 << 15)) & !(1 << 14));

    log!("xHCI: MSI-X enabled (vector {:#x})", XHCI_VECTOR);
}

pub fn init(ecam: &crate::mm::Mmio) -> Option<XhciController> {
    let pci_dev = PciDevice::find(ecam, 0x0C, 0x03, Some(0x30))?;
    log!("xHCI: found at PCI {:02x}:{:02x}.{}", pci_dev.bus, pci_dev.dev, pci_dev.func);

    let bar_addr = pci_dev.read_bar_64(0);
    pci_dev.enable_bus_master();
    log!("xHCI: BAR0={:#x}", bar_addr);

    let bar = crate::mm::paging::kernel().lock().as_mut().unwrap().map_mmio(bar_addr, 0x10000);

    setup_msix(&pci_dev);

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

    let bar_size = 0x10000u64;
    let op_base = bar.subregion(cap_length, bar_size - cap_length);
    let db_base = bar.subregion(db_offset, bar_size - db_offset);
    let rt_base = bar.subregion(rts_offset, bar_size - rts_offset);

    log!("xHCI: max_slots={} max_ports={} ctx_size={} pagesize={:#x}",
        max_slots, max_ports, context_size, op_base.read_u32(OP_PAGESIZE));

    let max_sp_hi = ((hcsparams2 >> 21) & 0x1F) as usize;
    let max_sp_lo = ((hcsparams2 >> 27) & 0x1F) as usize;
    let layout = Layout::new((max_sp_hi << 5) | max_sp_lo, max_slots);
    log!("xHCI: dma {} KiB: scratchpad={} device blocks={} of {} B (max_slots={})",
        layout.pool_size / 1024, layout.scratch_count, layout.dev_blocks, DEV_STRIDE, max_slots);
    *XHCI_DMA_POOL.lock() = Some(DmaPool::alloc(layout.pool_size));

    let usbcmd = op_base.read_u32(OP_USBCMD);
    if usbcmd & 1 != 0 {
        op_base.write_u32(OP_USBCMD, usbcmd & !1);
    }
    while op_base.read_u32(OP_USBSTS) & 1 == 0 {
        core::hint::spin_loop();
    }

    op_base.write_u32(OP_USBCMD, 1 << 1);
    while op_base.read_u32(OP_USBCMD) & (1 << 1) != 0 {
        core::hint::spin_loop();
    }
    while op_base.read_u32(OP_USBSTS) & (1 << 11) != 0 {
        core::hint::spin_loop();
    }
    log!("xHCI: controller reset");

    // MaxSlotsEn is what the driver can track, not what the controller can
    // offer: a conformant xHC then refuses Enable Slot past it rather than
    // handing back an id with nowhere to put its context.
    op_base.write_u32(OP_CONFIG, layout.dev_blocks as u32);

    let dma = dma();
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

    let cmd_ring_buf = dma.subslice(OFF_CMD_RING, PAGE);
    let mut link = Trb::ZERO;
    link.param = cmd_ring_buf.phys();
    link.control = TRB_LINK | (1 << 1);
    unsafe { write_volatile((cmd_ring_buf.base() as *mut Trb).add(RING_SIZE - 1), link); }
    op_base.write_u64(OP_CRCR, cmd_ring_buf.phys() | 1);

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

    // EP0 Ring (will be reset per device)
    let ep0_buf = dma.subslice(OFF_EP0_RING, PAGE);
    let mut ep0_link = Trb::ZERO;
    ep0_link.param = ep0_buf.phys();
    ep0_link.control = TRB_LINK | (1 << 1);
    unsafe { write_volatile((ep0_buf.base() as *mut Trb).add(RING_SIZE - 1), ep0_link); }

    // Start controller (R/S + INTE for interrupt delivery)
    op_base.write_u32(OP_USBCMD, 1 | (1 << 2));
    while op_base.read_u32(OP_USBSTS) & 1 != 0 {
        core::hint::spin_loop();
    }
    log!("xHCI: controller started");

    let mut ctrl = XhciController {
        db_base,
        rt_base,
        context_size,
        layout,
        cmd_ring: TrbRing::new(cmd_ring_buf),
        ep0_ring: TrbRing::new(ep0_buf),
        event_ring: evt_ring_buf.base() as *const Trb,
        event_head: 0,
        event_phase: true,
        active_slot: 0,
        devices: Vec::new(),
    };

    device::scan_ports(&mut ctrl, &op_base, max_ports);

    // A controller with no HID on it is still a controller, and returning it
    // is not a formality: it has been reset, started and armed with MSI-X, so
    // dropping it here leaves a live interrupter with nothing draining its
    // event ring. It is also the ordinary state of the target laptop, whose
    // keyboard is PS/2 and whose touchpad is I2C-HID — under metal-sim this
    // `None` reached `kernel_main`'s `.expect` and panicked the boot.
    if ctrl.devices.is_empty() {
        log!("xHCI: no HID devices found");
    }

    Some(ctrl)
}
