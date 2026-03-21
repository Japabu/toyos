mod device;
mod hid;

use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile, write_bytes};
use core::sync::atomic::{fence, Ordering};
use crate::mm::Mmio;
use super::pci::PciDevice;
use super::DmaPool;
use crate::log;
use crate::sync::Lock;

use hid::HidDevice;

// ---------------------------------------------------------------------------
// xHCI Capability Register offsets (from BAR0)
// ---------------------------------------------------------------------------
const CAP_CAPLENGTH:  u64 = 0x00; // u8
const CAP_HCSPARAMS1: u64 = 0x04; // u32
const CAP_HCSPARAMS2: u64 = 0x08; // u32
const CAP_HCCPARAMS1: u64 = 0x10; // u32
const CAP_DBOFF:      u64 = 0x14; // u32
const CAP_RTSOFF:     u64 = 0x18; // u32

// ---------------------------------------------------------------------------
// xHCI Operational Register offsets (from op_base = BAR0 + cap_length)
// ---------------------------------------------------------------------------
const OP_USBCMD:   u64 = 0x00;
const OP_USBSTS:   u64 = 0x04;
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

// ---------------------------------------------------------------------------
// Runtime Register offsets (from rt_base = BAR0 + rts_offset)
// Interrupter 0 starts at offset 0x20
// ---------------------------------------------------------------------------
const IR0_IMAN:   u64 = 0x20; // Interrupt Management (IP + IE)
const IR0_IMOD:   u64 = 0x24; // Interrupt Moderation
const IR0_ERSTSZ: u64 = 0x28;
const IR0_ERSTBA: u64 = 0x30; // 64-bit
const IR0_ERDP:   u64 = 0x38; // 64-bit

// MSI-X PCI capability ID
const PCI_CAP_MSIX: u8 = 0x11;
// xHCI interrupt vector
const XHCI_VECTOR: u8 = 0x21;

// ---------------------------------------------------------------------------
// TRB (Transfer Request Block) — 16 bytes
// ---------------------------------------------------------------------------
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

// Transfer TRB types
const TRB_NORMAL:       u32 = trb_type(1);
const TRB_SETUP_STAGE:  u32 = trb_type(2);
const TRB_DATA_STAGE:   u32 = trb_type(3);
const TRB_STATUS_STAGE: u32 = trb_type(4);
const TRB_LINK:         u32 = trb_type(6);

// Command TRB types
const TRB_ENABLE_SLOT:    u32 = trb_type(9);
const TRB_ADDRESS_DEVICE: u32 = trb_type(11);
const TRB_CONFIGURE_EP:   u32 = trb_type(12);

// Event TRB types (read from event ring, encoded in bits [15:10])
const EVENT_TRANSFER:     u32 = 32;
const EVENT_CMD_COMPLETE: u32 = 33;

// ---------------------------------------------------------------------------
// Ring sizes
// ---------------------------------------------------------------------------
const RING_SIZE: usize = 256; // TRBs per ring (one page = 256 * 16)

/// Event Ring Segment Table entry (16 bytes).
#[repr(C)]
struct ErstEntry {
    ring_base: u64,
    ring_size: u32,
    _reserved: u32,
}

// ---------------------------------------------------------------------------
// TRB Ring — shared enqueue logic for command, EP0, and interrupt rings
// ---------------------------------------------------------------------------
struct TrbRing {
    base: *mut Trb,
    base_phys: u64,
    tail: u16,
    cycle: bool,
}

impl TrbRing {
    fn new(base: *mut Trb, base_phys: crate::DmaAddr) -> Self {
        Self { base, base_phys: base_phys.raw(), tail: 0, cycle: true }
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

// ---------------------------------------------------------------------------
// DMA memory pool
// ---------------------------------------------------------------------------
//   Page 0:  DCBAA (Device Context Base Address Array)
//   Page 1:  Command Ring (256 TRBs)
//   Page 2:  Event Ring Segment Table
//   Page 3:  Event Ring (256 TRBs)
//   Page 4:  Output Context (slot 1)
//   Page 5:  Input Context (temporary, reused per device)
//   Page 6:  EP0 Transfer Ring (temporary, reused per device)
//   Page 7:  Keyboard Interrupt Ring
//   Page 8:  Data buffer + report buffers (kb@+512, mouse@+1024)
//   Page 9:  Scratchpad buffer array
//   Page 10: Output Context (slot 2)
//   Page 11: Mouse Interrupt Ring
//   Page 12: Output Context (slot 3)
const DMA_PAGES: usize = 13;
static XHCI_DMA_POOL: Lock<Option<DmaPool>> = Lock::new(None);

fn dma_phys(index: usize) -> crate::DmaAddr {
    XHCI_DMA_POOL.lock().as_ref().unwrap().page_phys(index)
}

fn dma_ptr(index: usize) -> *mut u8 {
    XHCI_DMA_POOL.lock().as_ref().unwrap().page_ptr(index)
}

// ---------------------------------------------------------------------------
// USB setup packet helper
// ---------------------------------------------------------------------------
fn setup_packet(bm_request_type: u8, b_request: u8, w_value: u16, w_index: u16, w_length: u16) -> u64 {
    (bm_request_type as u64)
        | ((b_request as u64) << 8)
        | ((w_value as u64) << 16)
        | ((w_index as u64) << 32)
        | ((w_length as u64) << 48)
}

// ---------------------------------------------------------------------------
// XhciController
// ---------------------------------------------------------------------------
// SAFETY: XhciController contains raw pointers to DMA memory that is valid
// for the lifetime of the controller. Access is serialized by the Lock.
unsafe impl Send for XhciController {}

pub struct XhciController {
    // Base addresses (MMIO)
    db_base: Mmio,
    rt_base: Mmio,

    // Capabilities
    context_size: usize, // 32 or 64

    // Shared rings
    cmd_ring: TrbRing,
    ep0_ring: TrbRing,

    // Event Ring
    event_ring: *const Trb,
    event_head: u16,
    event_phase: bool,

    // The slot_id used during init for EP0 doorbell targeting
    active_slot: u8,

    // HID devices (keyboard, mouse, etc.)
    devices: Vec<HidDevice>,
}

impl XhciController {
    // -------------------------------------------------------------------
    // Command ring: submit + wait
    // -------------------------------------------------------------------
    fn submit_command(&mut self, trb: Trb) {
        self.cmd_ring.enqueue(trb);
        fence(Ordering::Release);
        self.db_base.write_u32(0, 0);
    }

    fn wait_command(&mut self) -> (u32, u32) {
        loop {
            let event = unsafe { read_volatile(self.event_ring.add(self.event_head as usize)) };
            let cycle = (event.control & 1) != 0;
            if cycle != self.event_phase {
                core::hint::spin_loop();
                continue;
            }

            let trb_type = (event.control >> 10) & 0x3F;
            let code = (event.status >> 24) & 0xFF;
            let slot = (event.control >> 24) & 0xFF;

            self.advance_event_ring();

            if trb_type == EVENT_CMD_COMPLETE {
                return (code, slot);
            }
        }
    }

    // -------------------------------------------------------------------
    // Event ring dequeue pointer management
    // -------------------------------------------------------------------
    fn advance_event_ring(&mut self) {
        self.event_head = (self.event_head + 1) % RING_SIZE as u16;
        if self.event_head == 0 {
            self.event_phase = !self.event_phase;
        }
        let erdp = dma_phys(3).raw() + (self.event_head as u64) * 16;
        self.rt_base.write_u64(IR0_ERDP, erdp | (1 << 3)); // EHB clears interrupt pending
        self.rt_base.write_u32(IR0_IMAN, 3); // clear IP (W1C) + keep IE
    }

    // -------------------------------------------------------------------
    // EP0 transfer ring: enqueue TRBs + ring doorbell
    // -------------------------------------------------------------------
    fn enqueue_ep0(&mut self, trb: Trb) {
        self.ep0_ring.enqueue(trb);
    }

    fn ring_ep0_doorbell(&self) {
        fence(Ordering::Release);
        self.db_base.write_u32(self.active_slot as u64 * 4, 1);
    }

    fn wait_transfer(&mut self) -> u32 {
        loop {
            let event = unsafe { read_volatile(self.event_ring.add(self.event_head as usize)) };
            let cycle = (event.control & 1) != 0;
            if cycle != self.event_phase {
                core::hint::spin_loop();
                continue;
            }

            let trb_type = (event.control >> 10) & 0x3F;
            let code = (event.status >> 24) & 0xFF;
            self.advance_event_ring();

            if trb_type == EVENT_TRANSFER {
                return code;
            }
        }
    }

    // -------------------------------------------------------------------
    // Control transfer (Setup → [Data] → Status)
    // -------------------------------------------------------------------
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

    // -------------------------------------------------------------------
    // Reset EP0 ring for reuse between devices
    // -------------------------------------------------------------------
    fn reset_ep0_ring(&mut self) {
        unsafe { write_bytes(dma_ptr(6), 0, 4096); }
        let mut link = Trb::ZERO;
        link.param = dma_phys(6).raw();
        link.control = TRB_LINK | (1 << 1);
        unsafe { write_volatile((dma_ptr(6) as *mut Trb).add(RING_SIZE - 1), link); }
        self.ep0_ring = TrbRing::new(dma_ptr(6) as *mut Trb, dma_phys(6));
    }

    // -------------------------------------------------------------------
    // Poll: check event ring for completed interrupt transfers
    // -------------------------------------------------------------------
    pub fn poll(&mut self) {
        loop {
            let event = unsafe { read_volatile(self.event_ring.add(self.event_head as usize)) };
            let cycle = (event.control & 1) != 0;
            if cycle != self.event_phase {
                return;
            }

            let trb_type = (event.control >> 10) & 0x3F;
            let code = (event.status >> 24) & 0xFF;
            let slot = ((event.control >> 24) & 0xFF) as u8;
            self.advance_event_ring();

            if trb_type == EVENT_TRANSFER && (code == 1 || code == 13) {
                if let Some(dev) = self.devices.iter_mut().find(|d| d.slot_id == slot) {
                    dev.dispatch_report();
                    dev.requeue(&self.db_base);
                }
            }
        }
    }

    // -------------------------------------------------------------------
    // Write a context field into a context structure
    // -------------------------------------------------------------------
    fn write_ctx32(&self, ctx_base: *mut u8, slot_index: usize, dword: usize, val: u32) {
        let offset = (slot_index * self.context_size) + (dword * 4);
        unsafe { write_volatile(ctx_base.add(offset) as *mut u32, val); }
    }
}

// ---------------------------------------------------------------------------
// Global singleton (for sys_read polling)
// ---------------------------------------------------------------------------

static XHCI: Lock<Option<XhciController>> = Lock::new(None);

pub fn set_global(ctrl: XhciController) {
    *XHCI.lock() = Some(ctrl);
}

/// Process xHCI events only if an MSI-X interrupt fired.
pub fn poll_if_pending() {
    if crate::arch::idt::xhci_irq_pending() {
        let mut guard = XHCI.lock();
        if let Some(ctrl) = guard.as_mut() {
            ctrl.poll();
        }
    }
}

// ---------------------------------------------------------------------------
// MSI-X configuration
// ---------------------------------------------------------------------------

fn setup_msix(pci_dev: &PciDevice) {
    let cap = pci_dev.capabilities().find(|c| c.id() == PCI_CAP_MSIX);
    let cap = match cap {
        Some(c) => c,
        None => {
            log!("xHCI: no MSI-X capability, using polled mode");
            return;
        }
    };

    // Read MSI-X table location
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

// ---------------------------------------------------------------------------
// Main initialization
// ---------------------------------------------------------------------------

pub fn init(ecam: &crate::mm::Mmio) -> Option<XhciController> {
    let pci_dev = PciDevice::find(ecam, 0x0C, 0x03, Some(0x30))?;
    log!("xHCI: found at PCI {:02x}:{:02x}.{}", pci_dev.bus, pci_dev.dev, pci_dev.func);
    *XHCI_DMA_POOL.lock() = Some(DmaPool::alloc(DMA_PAGES));

    let bar_addr = pci_dev.read_bar_64(0);
    pci_dev.enable_bus_master();
    log!("xHCI: BAR0={:#x}", bar_addr);

    let bar = crate::mm::paging::kernel().lock().as_mut().unwrap().map_mmio(bar_addr, 0x10000);

    // Configure MSI-X
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

    log!("xHCI: max_slots={} max_ports={} ctx_size={}", max_slots, max_ports, context_size);

    // Halt controller
    let usbcmd = op_base.read_u32(OP_USBCMD);
    if usbcmd & 1 != 0 {
        op_base.write_u32(OP_USBCMD, usbcmd & !1);
    }
    while op_base.read_u32(OP_USBSTS) & 1 == 0 {
        core::hint::spin_loop();
    }

    // Reset controller
    op_base.write_u32(OP_USBCMD, 1 << 1);
    while op_base.read_u32(OP_USBCMD) & (1 << 1) != 0 {
        core::hint::spin_loop();
    }
    while op_base.read_u32(OP_USBSTS) & (1 << 11) != 0 {
        core::hint::spin_loop();
    }
    log!("xHCI: controller reset");

    op_base.write_u32(OP_CONFIG, max_slots as u32);

    // Zero DMA pages
    for i in 0..DMA_PAGES {
        unsafe { write_bytes(dma_ptr(i), 0, 4096); }
    }

    // Scratchpad buffers
    let max_sp_hi = ((hcsparams2 >> 21) & 0x1F) as usize;
    let max_sp_lo = ((hcsparams2 >> 27) & 0x1F) as usize;
    let max_scratchpad = (max_sp_hi << 5) | max_sp_lo;
    if max_scratchpad > 0 {
        let sp_array = dma_ptr(9) as *mut u64;
        unsafe { write_volatile(sp_array, dma_phys(9).raw() + 2048); }
        unsafe { write_volatile(dma_ptr(0) as *mut u64, dma_phys(9).raw()); }
        log!("xHCI: {} scratchpad buffers configured", max_scratchpad);
    }

    op_base.write_u64(OP_DCBAAP, dma_phys(0).raw());

    // Command Ring
    let cmd_ring = dma_ptr(1) as *mut Trb;
    let mut link = Trb::ZERO;
    link.param = dma_phys(1).raw();
    link.control = TRB_LINK | (1 << 1);
    unsafe { write_volatile(cmd_ring.add(RING_SIZE - 1), link); }
    op_base.write_u64(OP_CRCR, dma_phys(1).raw() | 1);

    // Event Ring
    let erst = dma_ptr(2) as *mut ErstEntry;
    unsafe {
        write_volatile(erst, ErstEntry {
            ring_base: dma_phys(3).raw(),
            ring_size: RING_SIZE as u32,
            _reserved: 0,
        });
    }
    rt_base.write_u32(IR0_ERSTSZ, 1);
    rt_base.write_u64(IR0_ERDP, dma_phys(3).raw());
    rt_base.write_u64(IR0_ERSTBA, dma_phys(2).raw());

    // Enable interrupter 0: set IE (bit 1) in IMAN, no moderation
    rt_base.write_u32(IR0_IMOD, 0);
    rt_base.write_u32(IR0_IMAN, 3); // clear IP (W1C) + set IE

    // EP0 Ring (will be reset per device)
    let ep0_ring = dma_ptr(6) as *mut Trb;
    let mut ep0_link = Trb::ZERO;
    ep0_link.param = dma_phys(6).raw();
    ep0_link.control = TRB_LINK | (1 << 1);
    unsafe { write_volatile(ep0_ring.add(RING_SIZE - 1), ep0_link); }

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
        cmd_ring: TrbRing::new(cmd_ring, dma_phys(1)),
        ep0_ring: TrbRing::new(ep0_ring, dma_phys(6)),
        event_ring: dma_ptr(3) as *const Trb,
        event_head: 0,
        event_phase: true,
        active_slot: 0,
        devices: Vec::new(),
    };

    // Scan all ports and initialize connected HID devices
    device::scan_ports(&mut ctrl, &op_base, max_ports);

    if ctrl.devices.is_empty() {
        log!("xHCI: no HID devices found");
        return None;
    }

    Some(ctrl)
}
