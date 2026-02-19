use core::ptr::{read_volatile, write_volatile, write_bytes, copy_nonoverlapping};
use core::sync::atomic::{fence, Ordering};
use crate::{pci, keyboard, log};
use alloc::format;

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
const _EVENT_PORT_STATUS: u32 = 34;

// ---------------------------------------------------------------------------
// Ring sizes
// ---------------------------------------------------------------------------
const RING_SIZE: usize = 256; // TRBs per ring (one page = 256 * 16)

// ---------------------------------------------------------------------------
// DMA memory pool (same pattern as NVMe)
// ---------------------------------------------------------------------------
//   Page 0: DCBAA (Device Context Base Address Array)
//   Page 1: Command Ring (256 TRBs)
//   Page 2: Event Ring Segment Table
//   Page 3: Event Ring (256 TRBs)
//   Page 4: Device Output Context
//   Page 5: Device Input Context
//   Page 6: EP0 Transfer Ring
//   Page 7: Interrupt IN Transfer Ring
//   Page 8: Data buffer (control transfer responses, HID reports)
//   Page 9: Scratchpad buffer array / spare
const DMA_PAGES: usize = 10;
const DMA_POOL_SIZE: usize = (DMA_PAGES + 1) * 4096;
static mut XHCI_DMA_POOL: [u8; DMA_POOL_SIZE] = [0; DMA_POOL_SIZE];

fn dma_page(index: usize) -> u64 {
    let raw = core::ptr::addr_of!(XHCI_DMA_POOL) as u64;
    let base = (raw + 4095) & !4095;
    base + (index as u64) * 4096
}

// ---------------------------------------------------------------------------
// MMIO helpers
// ---------------------------------------------------------------------------
fn mmio_read8(base: u64, offset: u64) -> u8 {
    unsafe { read_volatile((base + offset) as *const u8) }
}

fn mmio_read32(base: u64, offset: u64) -> u32 {
    unsafe { read_volatile((base + offset) as *const u32) }
}

fn mmio_write32(base: u64, offset: u64, val: u32) {
    unsafe { write_volatile((base + offset) as *mut u32, val) }
}

fn mmio_write64(base: u64, offset: u64, val: u64) {
    mmio_write32(base, offset, val as u32);
    mmio_write32(base, offset + 4, (val >> 32) as u32);
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
// USB HID usage ID → ASCII translation (boot protocol keyboard)
// ---------------------------------------------------------------------------
fn hid_usage_to_ascii(usage: u8, shift: bool) -> Option<u8> {
    match usage {
        // Letters: 0x04 ('a') through 0x1D ('z')
        0x04..=0x1D => {
            let base = b'a' + (usage - 0x04);
            Some(if shift { base - 32 } else { base })
        }
        // Numbers: 0x1E ('1') through 0x26 ('9')
        0x1E..=0x26 => {
            if shift {
                Some(match usage {
                    0x1E => b'!', 0x1F => b'@', 0x20 => b'#', 0x21 => b'$',
                    0x22 => b'%', 0x23 => b'^', 0x24 => b'&', 0x25 => b'*',
                    0x26 => b'(',
                    _ => return None,
                })
            } else {
                Some(b'1' + (usage - 0x1E))
            }
        }
        0x27 => Some(if shift { b')' } else { b'0' }),
        0x28 => Some(b'\n'),     // Enter
        0x29 => Some(0x1B),      // Escape
        0x2A => Some(0x08),      // Backspace
        0x2B => Some(b'\t'),     // Tab
        0x2C => Some(b' '),      // Space
        0x2D => Some(if shift { b'_' } else { b'-' }),
        0x2E => Some(if shift { b'+' } else { b'=' }),
        0x2F => Some(if shift { b'{' } else { b'[' }),
        0x30 => Some(if shift { b'}' } else { b']' }),
        0x31 => Some(if shift { b'|' } else { b'\\' }),
        0x33 => Some(if shift { b':' } else { b';' }),
        0x34 => Some(if shift { b'"' } else { b'\'' }),
        0x35 => Some(if shift { b'~' } else { b'`' }),
        0x36 => Some(if shift { b'<' } else { b',' }),
        0x37 => Some(if shift { b'>' } else { b'.' }),
        0x38 => Some(if shift { b'?' } else { b'/' }),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// XhciController
// ---------------------------------------------------------------------------
pub struct XhciController {
    // Base addresses (MMIO)
    db_base: u64,
    rt_base: u64,

    // Capabilities
    context_size: usize, // 32 or 64

    // Command Ring
    cmd_ring: *mut Trb,
    cmd_tail: u16,
    cmd_cycle: bool,

    // Event Ring
    event_ring: *const Trb,
    event_head: u16,
    event_phase: bool,

    // EP0 Transfer Ring
    ep0_ring: *mut Trb,
    ep0_tail: u16,
    ep0_cycle: bool,

    // Interrupt IN Transfer Ring
    int_ring: *mut Trb,
    int_tail: u16,
    int_cycle: bool,

    // Device state
    slot_id: u8,
    int_ep_dci: u8,

    // Keyboard state
    prev_report: [u8; 8],
}

impl XhciController {
    // -------------------------------------------------------------------
    // Command ring: submit + wait
    // -------------------------------------------------------------------
    fn submit_command(&mut self, mut trb: Trb) {
        if self.cmd_cycle {
            trb.control |= TRB_CYCLE;
        } else {
            trb.control &= !TRB_CYCLE;
        }

        unsafe { write_volatile(self.cmd_ring.add(self.cmd_tail as usize), trb); }
        self.cmd_tail += 1;

        // Wrap via Link TRB at last entry
        if self.cmd_tail as usize >= RING_SIZE - 1 {
            let mut link = Trb::ZERO;
            link.param = dma_page(1);
            link.control = TRB_LINK | (1 << 1); // Toggle Cycle
            if self.cmd_cycle { link.control |= TRB_CYCLE; }
            unsafe { write_volatile(self.cmd_ring.add(self.cmd_tail as usize), link); }
            self.cmd_tail = 0;
            self.cmd_cycle = !self.cmd_cycle;
        }

        fence(Ordering::Release);
        // Ring Host Controller doorbell (slot 0, target 0)
        mmio_write32(self.db_base, 0, 0);
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
            // Port status changes etc — skip
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
        mmio_write64(self.rt_base, IR0_ERDP, erdp | (1 << 3)); // EHB bit
    }

    // -------------------------------------------------------------------
    // EP0 transfer ring: enqueue TRBs + ring doorbell
    // -------------------------------------------------------------------
    fn enqueue_ep0(&mut self, mut trb: Trb) {
        if self.ep0_cycle {
            trb.control |= TRB_CYCLE;
        } else {
            trb.control &= !TRB_CYCLE;
        }
        unsafe { write_volatile(self.ep0_ring.add(self.ep0_tail as usize), trb); }
        self.ep0_tail += 1;

        if self.ep0_tail as usize >= RING_SIZE - 1 {
            let mut link = Trb::ZERO;
            link.param = dma_page(6);
            link.control = TRB_LINK | (1 << 1);
            if self.ep0_cycle { link.control |= TRB_CYCLE; }
            unsafe { write_volatile(self.ep0_ring.add(self.ep0_tail as usize), link); }
            self.ep0_tail = 0;
            self.ep0_cycle = !self.ep0_cycle;
        }
    }

    fn ring_ep0_doorbell(&self) {
        fence(Ordering::Release);
        // Doorbell for slot_id, target = 1 (DCI 1 = EP0)
        mmio_write32(self.db_base, self.slot_id as u64 * 4, 1);
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
            // Skip other events
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

        // TRT (Transfer Type): 0=No Data, 2=OUT Data, 3=IN Data
        let trt = if !has_data { 0u32 } else if is_in { 3 } else { 2 };

        // Setup Stage TRB
        let mut setup = Trb::ZERO;
        setup.param = setup_packet(bm_request_type, b_request, w_value, w_index, data_len);
        setup.status = 8; // always 8 for setup packet
        setup.control = TRB_SETUP_STAGE | (1 << 6) | (trt << 16); // IDT + TRT
        self.enqueue_ep0(setup);

        // Data Stage TRB
        if has_data {
            let mut data = Trb::ZERO;
            data.param = data_buf.unwrap();
            data.status = data_len as u32;
            let dir = if is_in { 1u32 << 16 } else { 0 };
            data.control = TRB_DATA_STAGE | dir;
            self.enqueue_ep0(data);
        }

        // Status Stage TRB (direction opposite to data)
        let mut status = Trb::ZERO;
        let status_dir = if has_data && is_in { 0 } else { 1u32 << 16 };
        status.control = TRB_STATUS_STAGE | (1 << 5) | status_dir; // IOC
        self.enqueue_ep0(status);

        self.ring_ep0_doorbell();
        self.wait_transfer()
    }

    // -------------------------------------------------------------------
    // Interrupt IN transfer ring
    // -------------------------------------------------------------------
    fn queue_interrupt_transfer(&mut self) {
        // Use offset 512 in data page for 8-byte HID report
        let report_buf = dma_page(8) + 512;

        let mut trb = Trb::ZERO;
        trb.param = report_buf;
        trb.status = 8; // 8-byte boot protocol report
        trb.control = TRB_NORMAL | (1 << 5); // IOC

        if self.int_cycle {
            trb.control |= TRB_CYCLE;
        } else {
            trb.control &= !TRB_CYCLE;
        }

        unsafe { write_volatile(self.int_ring.add(self.int_tail as usize), trb); }
        self.int_tail += 1;

        if self.int_tail as usize >= RING_SIZE - 1 {
            let mut link = Trb::ZERO;
            link.param = dma_page(7);
            link.control = TRB_LINK | (1 << 1);
            if self.int_cycle { link.control |= TRB_CYCLE; }
            unsafe { write_volatile(self.int_ring.add(self.int_tail as usize), link); }
            self.int_tail = 0;
            self.int_cycle = !self.int_cycle;
        }

        fence(Ordering::Release);
        // Ring doorbell for slot, target = interrupt endpoint DCI
        mmio_write32(self.db_base, self.slot_id as u64 * 4, self.int_ep_dci as u32);
    }

    // -------------------------------------------------------------------
    // Process HID boot protocol report
    // -------------------------------------------------------------------
    fn process_keyboard_report(&mut self) {
        let report_ptr = (dma_page(8) + 512) as *const u8;
        let mut report = [0u8; 8];
        unsafe { copy_nonoverlapping(report_ptr, report.as_mut_ptr(), 8); }

        let modifiers = report[0];
        let shift = (modifiers & 0x22) != 0; // Left Shift | Right Shift

        for i in 2..8 {
            let keycode = report[i];
            if keycode < 4 { continue; } // 0=none, 1=rollover, 2=POST fail, 3=undef
            // Only process newly pressed keys
            if !self.prev_report[2..8].contains(&keycode) {
                if let Some(ascii) = hid_usage_to_ascii(keycode, shift) {
                    keyboard::handle_key(ascii);
                }
            }
        }

        self.prev_report = report;
    }

    // -------------------------------------------------------------------
    // Poll: check event ring for completed interrupt transfers
    // -------------------------------------------------------------------
    pub fn poll(&mut self) {
        loop {
            let event = unsafe { read_volatile(self.event_ring.add(self.event_head as usize)) };
            let cycle = (event.control & 1) != 0;
            if cycle != self.event_phase {
                return; // no new events
            }

            let trb_type = (event.control >> 10) & 0x3F;
            let code = (event.status >> 24) & 0xFF;
            self.advance_event_ring();

            if trb_type == EVENT_TRANSFER {
                // 1 = Success, 13 = Short Packet (still valid for HID)
                if code == 1 || code == 13 {
                    self.process_keyboard_report();
                }
                // Re-queue for next report
                self.queue_interrupt_transfer();
            }
            // Port status changes, command completions — ignore during polling
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
// Initialization
// ---------------------------------------------------------------------------

fn find_xhci(ecam_base: u64) -> Option<(u8, u8, u8)> {
    // USB Controller: class=0x0C, subclass=0x03, prog_if=0x30 (xHCI)
    for bus in 0..=255u16 {
        for dev in 0..32u8 {
            let vendor_id = pci::ecam_read_u16(ecam_base, bus as u8, dev, 0, 0x00);
            if vendor_id == 0xFFFF { continue; }

            if check_xhci(ecam_base, bus as u8, dev, 0) {
                return Some((bus as u8, dev, 0));
            }

            let header_type = pci::ecam_read_u8(ecam_base, bus as u8, dev, 0, 0x0E);
            if header_type & 0x80 != 0 {
                for func in 1..=7u8 {
                    let vid = pci::ecam_read_u16(ecam_base, bus as u8, dev, func, 0x00);
                    if vid == 0xFFFF { continue; }
                    if check_xhci(ecam_base, bus as u8, dev, func) {
                        return Some((bus as u8, dev, func));
                    }
                }
            }
        }
    }
    None
}

fn check_xhci(ecam_base: u64, bus: u8, dev: u8, func: u8) -> bool {
    let class = pci::ecam_read_u8(ecam_base, bus, dev, func, 0x0B);
    let subclass = pci::ecam_read_u8(ecam_base, bus, dev, func, 0x0A);
    let prog_if = pci::ecam_read_u8(ecam_base, bus, dev, func, 0x09);
    class == 0x0C && subclass == 0x03 && prog_if == 0x30
}

fn max_packet_for_speed(speed: u8) -> u16 {
    match speed {
        2 => 8,    // Low Speed
        1 => 64,   // Full Speed
        3 => 64,   // High Speed
        4 => 512,  // Super Speed
        _ => 8,
    }
}

pub fn init(ecam_base: u64) -> Option<XhciController> {
    // 1. PCI discovery
    let (bus, dev, func) = find_xhci(ecam_base)?;
    log::println(&format!("xHCI: found at PCI {:02x}:{:02x}.{}", bus, dev, func));

    let bar = pci::read_bar0_64(ecam_base, bus, dev, func);
    pci::enable_bus_master(ecam_base, bus, dev, func);
    log::println(&format!("xHCI: BAR0={:#x}", bar));

    // 2. Read capability registers
    let cap_length = mmio_read8(bar, CAP_CAPLENGTH) as u64;
    let hcsparams1 = mmio_read32(bar, CAP_HCSPARAMS1);
    let hcsparams2 = mmio_read32(bar, CAP_HCSPARAMS2);
    let hccparams1 = mmio_read32(bar, CAP_HCCPARAMS1);
    let db_offset = (mmio_read32(bar, CAP_DBOFF) & !0x3) as u64;
    let rts_offset = (mmio_read32(bar, CAP_RTSOFF) & !0x1F) as u64;

    let max_slots = (hcsparams1 & 0xFF) as u8;
    let max_ports = ((hcsparams1 >> 24) & 0xFF) as u8;
    let csz = ((hccparams1 >> 2) & 1) != 0;
    let context_size: usize = if csz { 64 } else { 32 };

    let op_base = bar + cap_length;
    let db_base = bar + db_offset;
    let rt_base = bar + rts_offset;

    log::println(&format!("xHCI: max_slots={} max_ports={} ctx_size={}", max_slots, max_ports, context_size));

    // 3. Halt controller
    let usbcmd = mmio_read32(op_base, OP_USBCMD);
    if usbcmd & 1 != 0 {
        mmio_write32(op_base, OP_USBCMD, usbcmd & !1);
    }
    while mmio_read32(op_base, OP_USBSTS) & 1 == 0 {
        core::hint::spin_loop(); // Wait for HCH (Halted) bit
    }

    // 4. Reset controller
    mmio_write32(op_base, OP_USBCMD, 1 << 1); // HCRST
    while mmio_read32(op_base, OP_USBCMD) & (1 << 1) != 0 {
        core::hint::spin_loop();
    }
    while mmio_read32(op_base, OP_USBSTS) & (1 << 11) != 0 {
        core::hint::spin_loop(); // Wait for CNR (Controller Not Ready) to clear
    }
    log::println("xHCI: controller reset");

    // 5. Configure MaxSlotsEn
    mmio_write32(op_base, OP_CONFIG, max_slots as u32);

    // 6. Zero DMA pages
    for i in 0..DMA_PAGES {
        unsafe { write_bytes(dma_page(i) as *mut u8, 0, 4096); }
    }

    // 7. Set up DCBAA
    //    Check if scratchpad buffers are needed
    let max_sp_hi = ((hcsparams2 >> 21) & 0x1F) as usize;
    let max_sp_lo = ((hcsparams2 >> 27) & 0x1F) as usize;
    let max_scratchpad = (max_sp_hi << 5) | max_sp_lo;

    if max_scratchpad > 0 {
        // Scratchpad buffer array at page 9
        // Each entry is a 64-bit pointer to a scratchpad page.
        // For simplicity, we only support up to 1 scratchpad page (use the rest of page 9 as the actual buffer)
        let sp_array = dma_page(9) as *mut u64;
        // Point entry 0 to the second half of page 9 as a scratchpad buffer
        // (This is a simplification — real hardware may need more pages)
        unsafe { write_volatile(sp_array, dma_page(9) + 2048); }
        // DCBAA[0] = scratchpad buffer array pointer
        unsafe { write_volatile(dma_page(0) as *mut u64, dma_page(9)); }
        log::println(&format!("xHCI: {} scratchpad buffers configured", max_scratchpad));
    }

    mmio_write64(op_base, OP_DCBAAP, dma_page(0));

    // 8. Set up Command Ring with Link TRB at last entry
    let cmd_ring = dma_page(1) as *mut Trb;
    let mut link = Trb::ZERO;
    link.param = dma_page(1);
    link.control = TRB_LINK | (1 << 1); // TC (Toggle Cycle)
    unsafe { write_volatile(cmd_ring.add(RING_SIZE - 1), link); }

    // Write CRCR with ring base + RCS=1
    mmio_write64(op_base, OP_CRCR, dma_page(1) | 1);

    // 9. Set up Event Ring
    // Event Ring Segment Table entry at page 2:
    //   [0..7]  = ring segment base address (page 3)
    //   [8..11] = ring segment size (number of TRBs)
    let erst = dma_page(2) as *mut u8;
    unsafe {
        write_volatile(erst as *mut u64, dma_page(3));
        write_volatile(erst.add(8) as *mut u32, RING_SIZE as u32);
    }
    mmio_write32(rt_base, IR0_ERSTSZ, 1);
    mmio_write64(rt_base, IR0_ERDP, dma_page(3));
    mmio_write64(rt_base, IR0_ERSTBA, dma_page(2));

    // 10. Set up EP0 and Interrupt transfer rings with Link TRBs
    let ep0_ring = dma_page(6) as *mut Trb;
    let mut ep0_link = Trb::ZERO;
    ep0_link.param = dma_page(6);
    ep0_link.control = TRB_LINK | (1 << 1);
    unsafe { write_volatile(ep0_ring.add(RING_SIZE - 1), ep0_link); }

    let int_ring = dma_page(7) as *mut Trb;
    let mut int_link = Trb::ZERO;
    int_link.param = dma_page(7);
    int_link.control = TRB_LINK | (1 << 1);
    unsafe { write_volatile(int_ring.add(RING_SIZE - 1), int_link); }

    // 11. Start controller
    mmio_write32(op_base, OP_USBCMD, 1); // R/S = 1
    while mmio_read32(op_base, OP_USBSTS) & 1 != 0 {
        core::hint::spin_loop(); // Wait for HCH to clear (running)
    }
    log::println("xHCI: controller started");

    let event_ring = dma_page(3) as *const Trb;

    let mut ctrl = XhciController {
        db_base,
        rt_base,
        context_size,
        cmd_ring,
        cmd_tail: 0,
        cmd_cycle: true,
        event_ring,
        event_head: 0,
        event_phase: true,
        ep0_ring,
        ep0_tail: 0,
        ep0_cycle: true,
        int_ring,
        int_tail: 0,
        int_cycle: true,
        slot_id: 0,
        int_ep_dci: 0,
        prev_report: [0; 8],
    };

    // 12. Scan ports for a connected device
    let mut port_index: Option<u8> = None;
    for p in 0..max_ports {
        let portsc = mmio_read32(op_base, OP_PORT_BASE + p as u64 * PORT_REG_SIZE);
        if portsc & PORTSC_CCS != 0 {
            let speed = (portsc >> 10) & 0xF;
            log::println(&format!("xHCI: port {} connected, speed={}", p + 1, speed));
            port_index = Some(p);
            // Don't break — we want to find all devices and pick the right one later
        }
    }

    let port_idx = match port_index {
        Some(p) => p,
        None => {
            log::println("xHCI: no device connected");
            return None;
        }
    };

    // 13. Reset the port
    let portsc_off = OP_PORT_BASE + port_idx as u64 * PORT_REG_SIZE;
    let portsc = mmio_read32(op_base, portsc_off);
    mmio_write32(op_base, portsc_off, (portsc & !PORTSC_RW1C) | PORTSC_PR);

    // Wait for Port Reset Change
    loop {
        let ps = mmio_read32(op_base, portsc_off);
        if ps & PORTSC_PRC != 0 { break; }
        core::hint::spin_loop();
    }

    // Clear PRC
    let portsc = mmio_read32(op_base, portsc_off);
    mmio_write32(op_base, portsc_off, (portsc & !PORTSC_RW1C) | PORTSC_PRC);

    let portsc = mmio_read32(op_base, portsc_off);
    if portsc & PORTSC_PED == 0 {
        log::println("xHCI: port not enabled after reset");
        return None;
    }
    let speed = ((portsc >> 10) & 0xF) as u8;
    log::println(&format!("xHCI: port {} reset, speed={}", port_idx + 1, speed));

    // 14. Enable Slot
    let mut enable_slot = Trb::ZERO;
    enable_slot.control = TRB_ENABLE_SLOT;
    ctrl.submit_command(enable_slot);
    let (code, slot_id) = ctrl.wait_command();
    if code != 1 {
        log::println(&format!("xHCI: Enable Slot failed, code={}", code));
        return None;
    }
    ctrl.slot_id = slot_id as u8;
    log::println(&format!("xHCI: slot {} enabled", ctrl.slot_id));

    // 15. Address Device
    // Prepare Input Context at page 5
    let input_ctx = dma_page(5);
    unsafe { write_bytes(input_ctx as *mut u8, 0, 4096); }

    // Input Control Context (slot index 0): Add Context flags
    // Bit 0 = Slot Context (A0), Bit 1 = EP0 Context (A1)
    ctrl.write_ctx32(input_ctx, 0, 1, 0x3); // Add flags in dword 1

    // Slot Context (slot index 1):
    // dword 0: [19:0] Route String=0, [23:20] Speed, [27:27] MTT=0, [31:27] Context Entries=1
    let slot_dw0 = ((speed as u32) << 20) | (1u32 << 27);
    ctrl.write_ctx32(input_ctx, 1, 0, slot_dw0);
    // dword 1: [23:16] Root Hub Port Number (1-based)
    ctrl.write_ctx32(input_ctx, 1, 1, (port_idx as u32 + 1) << 16);

    // EP0 Context (slot index 2):
    let max_packet = max_packet_for_speed(speed);
    // dword 1: [2:1] CErr=3, [5:3] EP Type=4 (Control bidir), [15:0] also contains other bits
    let ep0_dw1 = (3u32 << 1) | (4u32 << 3) | ((max_packet as u32) << 16);
    ctrl.write_ctx32(input_ctx, 2, 1, ep0_dw1);
    // dword 2+3: TR Dequeue Pointer (64-bit) with DCS=1
    let ep0_dequeue = dma_page(6) | 1; // DCS = 1
    ctrl.write_ctx32(input_ctx, 2, 2, ep0_dequeue as u32);
    ctrl.write_ctx32(input_ctx, 2, 3, (ep0_dequeue >> 32) as u32);
    // dword 4: Average TRB Length = 8
    ctrl.write_ctx32(input_ctx, 2, 4, 8);

    // Point DCBAA[slot_id] to output context (page 4)
    let output_ctx = dma_page(4);
    unsafe { write_bytes(output_ctx as *mut u8, 0, 4096); }
    unsafe {
        let dcbaa = dma_page(0) as *mut u64;
        write_volatile(dcbaa.add(ctrl.slot_id as usize), output_ctx);
    }

    // Submit Address Device command
    let mut addr_dev = Trb::ZERO;
    addr_dev.param = input_ctx;
    addr_dev.control = TRB_ADDRESS_DEVICE | ((ctrl.slot_id as u32) << 24);
    ctrl.submit_command(addr_dev);
    let (code, _) = ctrl.wait_command();
    if code != 1 {
        log::println(&format!("xHCI: Address Device failed, code={}", code));
        return None;
    }
    log::println("xHCI: device addressed");

    // 16. GET_DESCRIPTOR (Device) — 18 bytes
    let data_buf = dma_page(8);
    unsafe { write_bytes(data_buf as *mut u8, 0, 256); }
    let code = ctrl.control_transfer(0x80, 0x06, 0x0100, 0, Some(data_buf), 18);
    if code != 1 && code != 13 {
        log::println(&format!("xHCI: GET_DESCRIPTOR(Device) failed, code={}", code));
        return None;
    }

    let (dev_class, vendor_id, product_id) = unsafe {
        let buf = data_buf as *const u8;
        let dev_class = read_volatile(buf.add(4));
        let vendor_id = read_volatile(buf.add(8)) as u16
            | (read_volatile(buf.add(9)) as u16) << 8;
        let product_id = read_volatile(buf.add(10)) as u16
            | (read_volatile(buf.add(11)) as u16) << 8;
        (dev_class, vendor_id, product_id)
    };
    log::println(&format!("xHCI: device class={:#x} vendor={:04x} product={:04x}", dev_class, vendor_id, product_id));

    // 17. GET_DESCRIPTOR (Configuration) — request 256 bytes
    unsafe { write_bytes(data_buf as *mut u8, 0, 256); }
    let code = ctrl.control_transfer(0x80, 0x06, 0x0200, 0, Some(data_buf), 256);
    if code != 1 && code != 13 {
        log::println(&format!("xHCI: GET_DESCRIPTOR(Config) failed, code={}", code));
        return None;
    }

    // Parse configuration descriptor to find HID keyboard interface + interrupt IN endpoint
    let (config_val, iface_num, ep_addr, ep_max_packet, ep_interval) = unsafe {
        let buf = data_buf as *const u8;
        let total_len = (read_volatile(buf.add(2)) as usize)
            | (read_volatile(buf.add(3)) as usize) << 8;
        let config_val = read_volatile(buf.add(5));

        let mut found_keyboard = false;
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
                4 => {
                    // Interface Descriptor
                    if offset + 9 <= len {
                        let intf_class = read_volatile(buf.add(offset + 5));
                        let intf_subclass = read_volatile(buf.add(offset + 6));
                        let intf_protocol = read_volatile(buf.add(offset + 7));
                        if intf_class == 3 && intf_subclass == 1 && intf_protocol == 1 {
                            // HID Boot Keyboard
                            found_keyboard = true;
                            iface_num = read_volatile(buf.add(offset + 2));
                        } else {
                            found_keyboard = false;
                        }
                    }
                }
                5 => {
                    // Endpoint Descriptor
                    if found_keyboard && offset + 7 <= len {
                        let addr = read_volatile(buf.add(offset + 2));
                        if addr & 0x80 != 0 {
                            // IN endpoint
                            ep_addr = addr;
                            ep_max_packet = read_volatile(buf.add(offset + 4)) as u16
                                | (read_volatile(buf.add(offset + 5)) as u16) << 8;
                            ep_interval = read_volatile(buf.add(offset + 6));
                        }
                    }
                }
                _ => {}
            }
            offset += desc_len;
        }

        (config_val, iface_num, ep_addr, ep_max_packet, ep_interval)
    };

    if ep_addr == 0 {
        log::println("xHCI: no HID keyboard interface found");
        return None;
    }
    let ep_num = ep_addr & 0x0F;
    let int_ep_dci = ep_num * 2 + 1; // IN endpoint DCI = ep_num * 2 + 1
    ctrl.int_ep_dci = int_ep_dci;

    log::println(&format!(
        "xHCI: HID keyboard iface={} ep={:#x} max_pkt={} interval={} dci={}",
        iface_num, ep_addr, ep_max_packet, ep_interval, int_ep_dci
    ));

    // 18. SET_CONFIGURATION
    let code = ctrl.control_transfer(0x00, 0x09, config_val as u16, 0, None, 0);
    if code != 1 {
        log::println(&format!("xHCI: SET_CONFIGURATION failed, code={}", code));
        return None;
    }
    log::println("xHCI: configuration set");

    // 19. SET_PROTOCOL (boot protocol, wValue=0)
    let code = ctrl.control_transfer(0x21, 0x0B, 0, iface_num as u16, None, 0);
    if code != 1 {
        log::println(&format!("xHCI: SET_PROTOCOL failed, code={}", code));
        // Non-fatal: some devices default to boot protocol
    }

    // 20. Configure Endpoint (interrupt IN)
    let input_ctx = dma_page(5);
    unsafe { write_bytes(input_ctx as *mut u8, 0, 4096); }

    // Input Control Context: add the interrupt endpoint
    ctrl.write_ctx32(input_ctx, 0, 1, 1u32 << (int_ep_dci as u32)); // Add flag for this DCI
    // Also need to set Slot Context with updated Context Entries
    ctrl.write_ctx32(input_ctx, 0, 1,
        (1u32 << (int_ep_dci as u32)) | 1); // A0 (slot) + endpoint DCI

    // Slot Context: update Context Entries to include the new endpoint
    // Context Entries = max DCI that is valid
    let slot_dw0 = ((speed as u32) << 20) | ((int_ep_dci as u32) << 27);
    ctrl.write_ctx32(input_ctx, 1, 0, slot_dw0);
    ctrl.write_ctx32(input_ctx, 1, 1, (port_idx as u32 + 1) << 16);

    // Endpoint Context for interrupt IN endpoint
    let ep_ctx_index = int_ep_dci as usize + 1; // +1 because index 0 = input control, 1 = slot
    // dword 0: [23:16] Interval, [9:8] Mult=0, [4:2] LSA=0
    // For FS/LS: interval = ep_interval (in frames), xHCI expects interval in 2^(Interval) * 125us
    // For HID keyboard in QEMU, just use the endpoint interval
    let interval_val = if ep_interval == 0 { 0u32 } else {
        // Convert bInterval to xHCI interval exponent
        // For FS/LS endpoints: interval = ep_interval * 8 (in 125us units)
        // For HS: interval = ep_interval - 1
        if speed <= 2 {
            // Full/Low speed: bInterval is in ms (1-255), xHCI wants 2^(Interval)*125us
            // log2(bInterval * 8) approximately
            let frames = (ep_interval as u32) * 8;
            let mut exp = 0u32;
            let mut v = frames;
            while v > 1 { v >>= 1; exp += 1; }
            exp
        } else {
            // High/Super speed: bInterval is already 2^(bInterval-1) * 125us
            (ep_interval - 1) as u32
        }
    };
    let ep_dw0 = interval_val << 16;
    ctrl.write_ctx32(input_ctx, ep_ctx_index, 0, ep_dw0);

    // dword 1: [2:1] CErr=3, [5:3] EP Type=7 (Interrupt IN), [15:0] Max Packet Size
    let ep_dw1 = (3u32 << 1) | (7u32 << 3) | ((ep_max_packet as u32) << 16);
    ctrl.write_ctx32(input_ctx, ep_ctx_index, 1, ep_dw1);

    // dword 2+3: TR Dequeue Pointer with DCS=1
    let int_dequeue = dma_page(7) | 1;
    ctrl.write_ctx32(input_ctx, ep_ctx_index, 2, int_dequeue as u32);
    ctrl.write_ctx32(input_ctx, ep_ctx_index, 3, (int_dequeue >> 32) as u32);

    // dword 4: Average TRB Length = 8
    ctrl.write_ctx32(input_ctx, ep_ctx_index, 4, 8);

    // Submit Configure Endpoint command
    let mut config_ep = Trb::ZERO;
    config_ep.param = input_ctx;
    config_ep.control = TRB_CONFIGURE_EP | ((ctrl.slot_id as u32) << 24);
    ctrl.submit_command(config_ep);
    let (code, _) = ctrl.wait_command();
    if code != 1 {
        log::println(&format!("xHCI: Configure Endpoint failed, code={}", code));
        return None;
    }
    log::println("xHCI: endpoint configured");

    // 21. Queue initial interrupt IN transfer
    ctrl.queue_interrupt_transfer();

    log::println("xHCI: USB keyboard ready");
    Some(ctrl)
}
