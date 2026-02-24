use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile, write_bytes, copy_nonoverlapping};
use core::sync::atomic::{fence, Ordering};
use super::mmio::Mmio;
use super::pci::PciDevice;
use super::DmaPool;
use crate::{keyboard, mouse, log};
use crate::sync::SyncCell;

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
const IR0_ERSTSZ: u64 = 0x28;
const IR0_ERSTBA: u64 = 0x30; // 64-bit
const IR0_ERDP:   u64 = 0x38; // 64-bit

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
    fn new(base: *mut Trb, base_phys: u64) -> Self {
        Self { base, base_phys, tail: 0, cycle: true }
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
static XHCI_DMA_POOL: SyncCell<DmaPool<DMA_PAGES>> = SyncCell::new(DmaPool::new());

fn dma_page(index: usize) -> u64 {
    XHCI_DMA_POOL.get().page_addr(index)
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
// Per-HID-device state
// ---------------------------------------------------------------------------
#[derive(Clone, Copy, PartialEq)]
enum HidType {
    Keyboard,
    Mouse,
}

struct HidDevice {
    slot_id: u8,
    int_ep_dci: u8,
    int_ring: TrbRing,
    report_buf: u64,
    report_size: u32,
    hid_type: HidType,
}

impl HidDevice {
    fn dispatch_report(&self) {
        let mut buf = [0u8; 8];
        let size = self.report_size as usize;
        unsafe { copy_nonoverlapping(self.report_buf as *const u8, buf.as_mut_ptr(), size); }
        match self.hid_type {
            HidType::Keyboard => keyboard::handle_report(&buf[..size]),
            HidType::Mouse => mouse::handle_report(&buf[..size]),
        }
    }

    fn requeue(&mut self, db_base: &Mmio) {
        let mut trb = Trb::ZERO;
        trb.param = self.report_buf;
        trb.status = self.report_size;
        trb.control = TRB_NORMAL | (1 << 5); // IOC
        self.int_ring.enqueue(trb);
        fence(Ordering::Release);
        db_base.write_u32(self.slot_id as u64 * 4, self.int_ep_dci as u32);
    }
}

// ---------------------------------------------------------------------------
// XhciController
// ---------------------------------------------------------------------------
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
        let erdp = dma_page(3) + (self.event_head as u64) * 16;
        self.rt_base.write_u64(IR0_ERDP, erdp | (1 << 3));
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
        unsafe { write_bytes(dma_page(6) as *mut u8, 0, 4096); }
        let mut link = Trb::ZERO;
        link.param = dma_page(6);
        link.control = TRB_LINK | (1 << 1);
        unsafe { write_volatile((dma_page(6) as *mut Trb).add(RING_SIZE - 1), link); }
        self.ep0_ring = TrbRing::new(dma_page(6) as *mut Trb, dma_page(6));
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
    fn write_ctx32(&self, ctx_base: u64, slot_index: usize, dword: usize, val: u32) {
        let offset = (slot_index * self.context_size) + (dword * 4);
        unsafe { write_volatile((ctx_base + offset as u64) as *mut u32, val); }
    }
}

// ---------------------------------------------------------------------------
// Global singleton (for sys_read polling)
// ---------------------------------------------------------------------------

static XHCI: SyncCell<Option<XhciController>> = SyncCell::new(None);

pub fn set_global(ctrl: XhciController) {
    *XHCI.get_mut() = Some(ctrl);
}

pub fn poll_global() {
    if let Some(ctrl) = XHCI.get_mut() {
        ctrl.poll();
    }
}

// ---------------------------------------------------------------------------
// Initialization helpers
// ---------------------------------------------------------------------------

fn max_packet_for_speed(speed: u8) -> u16 {
    match speed {
        2 => 8,    // Low Speed
        1 => 64,   // Full Speed
        3 => 64,   // High Speed
        4 => 512,  // Super Speed
        _ => 8,
    }
}

/// Map slot_id (1-based, assigned by controller) to DMA page for output context.
fn output_ctx_page(slot_id: u8) -> usize {
    match slot_id {
        1 => 4,
        2 => 10,
        3 => 12,
        _ => panic!("xHCI: too many USB slots (max 3)"),
    }
}

/// Result of parsing a USB device's configuration descriptor for HID interfaces.
struct HidInterfaceInfo {
    protocol: HidType,
    config_val: u8,
    iface_num: u8,
    ep_addr: u8,
    ep_max_packet: u16,
    ep_interval: u8,
}

/// Initialize and configure one USB device on a port.
/// Returns the HID info if this is a keyboard or mouse, None otherwise.
fn init_device(ctrl: &mut XhciController, op_base: &Mmio, port_idx: u8) -> Option<HidInterfaceInfo> {
    // Reset port
    let portsc_off = OP_PORT_BASE + port_idx as u64 * PORT_REG_SIZE;
    let portsc = op_base.read_u32(portsc_off);
    op_base.write_u32(portsc_off, (portsc & !PORTSC_RW1C) | PORTSC_PR);

    loop {
        let ps = op_base.read_u32(portsc_off);
        if ps & PORTSC_PRC != 0 { break; }
        core::hint::spin_loop();
    }
    let portsc = op_base.read_u32(portsc_off);
    op_base.write_u32(portsc_off, (portsc & !PORTSC_RW1C) | PORTSC_PRC);

    let portsc = op_base.read_u32(portsc_off);
    if portsc & PORTSC_PED == 0 {
        log!("xHCI: port {} not enabled after reset", port_idx + 1);
        return None;
    }
    let speed = ((portsc >> 10) & 0xF) as u8;
    log!("xHCI: port {} reset, speed={}", port_idx + 1, speed);

    // Enable Slot
    let mut enable_slot = Trb::ZERO;
    enable_slot.control = TRB_ENABLE_SLOT;
    ctrl.submit_command(enable_slot);
    let (code, slot_id) = ctrl.wait_command();
    if code != 1 {
        log!("xHCI: Enable Slot failed, code={}", code);
        return None;
    }
    let slot_id = slot_id as u8;
    ctrl.active_slot = slot_id;
    log!("xHCI: slot {} enabled", slot_id);

    // Reset EP0 ring for this device
    ctrl.reset_ep0_ring();

    // Address Device
    let input_ctx = dma_page(5);
    unsafe { write_bytes(input_ctx as *mut u8, 0, 4096); }

    ctrl.write_ctx32(input_ctx, 0, 1, 0x3); // Add Slot + EP0
    let slot_dw0 = ((speed as u32) << 20) | (1u32 << 27);
    ctrl.write_ctx32(input_ctx, 1, 0, slot_dw0);
    ctrl.write_ctx32(input_ctx, 1, 1, (port_idx as u32 + 1) << 16);

    let max_packet = max_packet_for_speed(speed);
    let ep0_dw1 = (3u32 << 1) | (4u32 << 3) | ((max_packet as u32) << 16);
    ctrl.write_ctx32(input_ctx, 2, 1, ep0_dw1);
    let ep0_dequeue = dma_page(6) | 1;
    ctrl.write_ctx32(input_ctx, 2, 2, ep0_dequeue as u32);
    ctrl.write_ctx32(input_ctx, 2, 3, (ep0_dequeue >> 32) as u32);
    ctrl.write_ctx32(input_ctx, 2, 4, 8);

    let output_ctx = dma_page(output_ctx_page(slot_id));
    unsafe { write_bytes(output_ctx as *mut u8, 0, 4096); }
    unsafe {
        let dcbaa = dma_page(0) as *mut u64;
        write_volatile(dcbaa.add(slot_id as usize), output_ctx);
    }

    let mut addr_dev = Trb::ZERO;
    addr_dev.param = input_ctx;
    addr_dev.control = TRB_ADDRESS_DEVICE | ((slot_id as u32) << 24);
    ctrl.submit_command(addr_dev);
    let (code, _) = ctrl.wait_command();
    if code != 1 {
        log!("xHCI: Address Device failed, code={}", code);
        return None;
    }
    log!("xHCI: device addressed");

    // GET_DESCRIPTOR (Device)
    let data_buf = dma_page(8);
    unsafe { write_bytes(data_buf as *mut u8, 0, 256); }
    let code = ctrl.control_transfer(0x80, 0x06, 0x0100, 0, Some(data_buf), 18);
    if code != 1 && code != 13 {
        log!("xHCI: GET_DESCRIPTOR(Device) failed, code={}", code);
        return None;
    }

    let (dev_class, vendor_id, product_id) = unsafe {
        let buf = data_buf as *const u8;
        (
            read_volatile(buf.add(4)),
            read_volatile(buf.add(8)) as u16 | (read_volatile(buf.add(9)) as u16) << 8,
            read_volatile(buf.add(10)) as u16 | (read_volatile(buf.add(11)) as u16) << 8,
        )
    };
    log!("xHCI: device class={:#x} vendor={:04x} product={:04x}", dev_class, vendor_id, product_id);

    // GET_DESCRIPTOR (Configuration)
    unsafe { write_bytes(data_buf as *mut u8, 0, 256); }
    let code = ctrl.control_transfer(0x80, 0x06, 0x0200, 0, Some(data_buf), 256);
    if code != 1 && code != 13 {
        log!("xHCI: GET_DESCRIPTOR(Config) failed, code={}", code);
        return None;
    }

    // Parse configuration descriptor for HID boot keyboard or mouse
    let info = unsafe {
        let buf = data_buf as *const u8;
        let total_len = (read_volatile(buf.add(2)) as usize)
            | (read_volatile(buf.add(3)) as usize) << 8;
        let config_val = read_volatile(buf.add(5));

        let mut found_protocol: Option<HidType> = None;
        let mut iface_num: u8 = 0;
        let mut ep_addr: u8 = 0;
        let mut ep_max_packet: u16 = 0;
        let mut ep_interval: u8 = 0;

        let mut offset = 0usize;
        let len = total_len.min(256);
        while offset + 2 <= len {
            let desc_len = read_volatile(buf.add(offset)) as usize;
            let desc_type = read_volatile(buf.add(offset + 1));
            if desc_len == 0 { break; }

            match desc_type {
                4 if offset + 9 <= len => {
                    let intf_class = read_volatile(buf.add(offset + 5));
                    let intf_subclass = read_volatile(buf.add(offset + 6));
                    let intf_protocol = read_volatile(buf.add(offset + 7));
                    if intf_class == 3 && intf_subclass == 1 {
                        found_protocol = match intf_protocol {
                            1 => Some(HidType::Keyboard),
                            2 => Some(HidType::Mouse),
                            _ => None,
                        };
                        if found_protocol.is_some() {
                            iface_num = read_volatile(buf.add(offset + 2));
                        }
                    } else {
                        found_protocol = None;
                    }
                }
                5 if found_protocol.is_some() && offset + 7 <= len => {
                    let addr = read_volatile(buf.add(offset + 2));
                    if addr & 0x80 != 0 && ep_addr == 0 {
                        ep_addr = addr;
                        ep_max_packet = read_volatile(buf.add(offset + 4)) as u16
                            | (read_volatile(buf.add(offset + 5)) as u16) << 8;
                        ep_interval = read_volatile(buf.add(offset + 6));
                    }
                }
                _ => {}
            }
            offset += desc_len;
        }

        if ep_addr != 0 && found_protocol.is_some() {
            Some(HidInterfaceInfo {
                protocol: found_protocol.unwrap(),
                config_val,
                iface_num,
                ep_addr,
                ep_max_packet,
                ep_interval,
            })
        } else {
            None
        }
    };

    let info = match info {
        Some(i) => i,
        None => {
            log!("xHCI: no HID boot interface found, skipping");
            return None;
        }
    };

    let kind = match info.protocol {
        HidType::Keyboard => "keyboard",
        HidType::Mouse => "mouse",
    };
    let ep_num = info.ep_addr & 0x0F;
    let int_ep_dci = ep_num * 2 + 1;
    log!("xHCI: HID {} iface={} ep={:#x} max_pkt={} interval={} dci={}",
        kind, info.iface_num, info.ep_addr, info.ep_max_packet, info.ep_interval, int_ep_dci);

    // SET_CONFIGURATION
    let code = ctrl.control_transfer(0x00, 0x09, info.config_val as u16, 0, None, 0);
    if code != 1 {
        log!("xHCI: SET_CONFIGURATION failed, code={}", code);
        return None;
    }
    log!("xHCI: configuration set");

    // SET_PROTOCOL (boot protocol)
    let code = ctrl.control_transfer(0x21, 0x0B, 0, info.iface_num as u16, None, 0);
    if code != 1 {
        log!("xHCI: SET_PROTOCOL failed, code={}", code);
    }

    // Choose interrupt ring and report buffer based on device type
    let (int_ring_page, report_buf) = match info.protocol {
        HidType::Keyboard => (7, dma_page(8) + 512),
        HidType::Mouse => (11, dma_page(8) + 1024),
    };

    // Set up interrupt ring link TRB
    let int_ring_ptr = dma_page(int_ring_page) as *mut Trb;
    unsafe { write_bytes(int_ring_ptr as *mut u8, 0, 4096); }
    let mut int_link = Trb::ZERO;
    int_link.param = dma_page(int_ring_page);
    int_link.control = TRB_LINK | (1 << 1);
    unsafe { write_volatile(int_ring_ptr.add(RING_SIZE - 1), int_link); }

    // Configure Endpoint
    let input_ctx = dma_page(5);
    unsafe { write_bytes(input_ctx as *mut u8, 0, 4096); }

    ctrl.write_ctx32(input_ctx, 0, 1, (1u32 << (int_ep_dci as u32)) | 1);

    let slot_dw0 = ((speed as u32) << 20) | ((int_ep_dci as u32) << 27);
    ctrl.write_ctx32(input_ctx, 1, 0, slot_dw0);
    ctrl.write_ctx32(input_ctx, 1, 1, (port_idx as u32 + 1) << 16);

    let ep_ctx_index = int_ep_dci as usize + 1;
    let interval_val = if info.ep_interval == 0 { 0u32 } else if speed <= 2 {
        let frames = (info.ep_interval as u32) * 8;
        let mut exp = 0u32;
        let mut v = frames;
        while v > 1 { v >>= 1; exp += 1; }
        exp
    } else {
        (info.ep_interval - 1) as u32
    };
    ctrl.write_ctx32(input_ctx, ep_ctx_index, 0, interval_val << 16);

    let ep_dw1 = (3u32 << 1) | (7u32 << 3) | ((info.ep_max_packet as u32) << 16);
    ctrl.write_ctx32(input_ctx, ep_ctx_index, 1, ep_dw1);

    let int_dequeue = dma_page(int_ring_page) | 1;
    ctrl.write_ctx32(input_ctx, ep_ctx_index, 2, int_dequeue as u32);
    ctrl.write_ctx32(input_ctx, ep_ctx_index, 3, (int_dequeue >> 32) as u32);
    ctrl.write_ctx32(input_ctx, ep_ctx_index, 4, 8);

    let mut config_ep = Trb::ZERO;
    config_ep.param = input_ctx;
    config_ep.control = TRB_CONFIGURE_EP | ((slot_id as u32) << 24);
    ctrl.submit_command(config_ep);
    let (code, _) = ctrl.wait_command();
    if code != 1 {
        log!("xHCI: Configure Endpoint failed, code={}", code);
        return None;
    }
    log!("xHCI: endpoint configured");

    // Store device and queue initial interrupt transfer
    let report_size = match info.protocol {
        HidType::Keyboard => 8,
        HidType::Mouse => 4,
    };
    let mut dev = HidDevice {
        slot_id,
        int_ep_dci,
        int_ring: TrbRing::new(int_ring_ptr, dma_page(int_ring_page)),
        report_buf,
        report_size,
        hid_type: info.protocol,
    };

    dev.requeue(&ctrl.db_base);
    log!("xHCI: USB {} ready", kind);
    ctrl.devices.push(dev);

    Some(info)
}

// ---------------------------------------------------------------------------
// Main initialization
// ---------------------------------------------------------------------------

pub fn init(ecam_base: u64) -> Option<XhciController> {
    let pci_dev = PciDevice::find(ecam_base, 0x0C, 0x03, Some(0x30))?;
    log!("xHCI: found at PCI {:02x}:{:02x}.{}", pci_dev.bus, pci_dev.dev, pci_dev.func);

    let bar = Mmio::new(pci_dev.read_bar_64(0));
    pci_dev.enable_bus_master();
    log!("xHCI: BAR0={:#x}", bar.addr());

    crate::arch::paging::map_kernel(bar.addr(), 0x10000);

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

    let op_base = bar.offset(cap_length);
    let db_base = bar.offset(db_offset);
    let rt_base = bar.offset(rts_offset);

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
        unsafe { write_bytes(dma_page(i) as *mut u8, 0, 4096); }
    }

    // Scratchpad buffers
    let max_sp_hi = ((hcsparams2 >> 21) & 0x1F) as usize;
    let max_sp_lo = ((hcsparams2 >> 27) & 0x1F) as usize;
    let max_scratchpad = (max_sp_hi << 5) | max_sp_lo;
    if max_scratchpad > 0 {
        let sp_array = dma_page(9) as *mut u64;
        unsafe { write_volatile(sp_array, dma_page(9) + 2048); }
        unsafe { write_volatile(dma_page(0) as *mut u64, dma_page(9)); }
        log!("xHCI: {} scratchpad buffers configured", max_scratchpad);
    }

    op_base.write_u64(OP_DCBAAP, dma_page(0));

    // Command Ring
    let cmd_ring = dma_page(1) as *mut Trb;
    let mut link = Trb::ZERO;
    link.param = dma_page(1);
    link.control = TRB_LINK | (1 << 1);
    unsafe { write_volatile(cmd_ring.add(RING_SIZE - 1), link); }
    op_base.write_u64(OP_CRCR, dma_page(1) | 1);

    // Event Ring
    let erst = dma_page(2) as *mut u8;
    unsafe {
        write_volatile(erst as *mut u64, dma_page(3));
        write_volatile(erst.add(8) as *mut u32, RING_SIZE as u32);
    }
    rt_base.write_u32(IR0_ERSTSZ, 1);
    rt_base.write_u64(IR0_ERDP, dma_page(3));
    rt_base.write_u64(IR0_ERSTBA, dma_page(2));

    // EP0 Ring (will be reset per device)
    let ep0_ring = dma_page(6) as *mut Trb;
    let mut ep0_link = Trb::ZERO;
    ep0_link.param = dma_page(6);
    ep0_link.control = TRB_LINK | (1 << 1);
    unsafe { write_volatile(ep0_ring.add(RING_SIZE - 1), ep0_link); }

    // Start controller
    op_base.write_u32(OP_USBCMD, 1);
    while op_base.read_u32(OP_USBSTS) & 1 != 0 {
        core::hint::spin_loop();
    }
    log!("xHCI: controller started");

    let mut ctrl = XhciController {
        db_base,
        rt_base,
        context_size,
        cmd_ring: TrbRing::new(cmd_ring, dma_page(1)),
        ep0_ring: TrbRing::new(ep0_ring, dma_page(6)),
        event_ring: dma_page(3) as *const Trb,
        event_head: 0,
        event_phase: true,
        active_slot: 0,
        devices: Vec::new(),
    };

    // Scan all ports and initialize connected HID devices
    for p in 0..max_ports {
        let portsc = op_base.read_u32(OP_PORT_BASE + p as u64 * PORT_REG_SIZE);
        if portsc & PORTSC_CCS != 0 {
            log!("xHCI: port {} connected, speed={}", p + 1, (portsc >> 10) & 0xF);
            init_device(&mut ctrl, &op_base, p);
        }
    }

    if ctrl.devices.is_empty() {
        log!("xHCI: no HID devices found");
        return None;
    }

    Some(ctrl)
}
