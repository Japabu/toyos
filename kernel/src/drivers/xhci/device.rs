use core::ptr::{write_volatile, write_bytes};

use crate::log;
use super::{Mmio, Trb, TrbRing, XhciController, PAGE};
use super::{OFF_DCBAA, OFF_INPUT_CTX, OFF_DATA_BUF};
use super::{DEV_INT_RING, DEV_EP0_RING, DEV_OUT_CTX, DEV_REPORT};
use super::{TRB_ENABLE_SLOT, TRB_ADDRESS_DEVICE, TRB_CONFIGURE_EP, CC_SUCCESS, CC_SHORT_PACKET};
use super::{OP_PORT_BASE, PORT_REG_SIZE, PORTSC_CCS, PORTSC_PED, PORTSC_PR, PORTSC_PRC, PORTSC_RW1C};
use super::hid::{HidType, HidRole, HidDevice};
use super::msc::MscInterface;

/// How much of a configuration descriptor the driver reads and parses.
///
/// It is also the size of the GET_DESCRIPTOR request, so a device cannot make
/// the parser walk past it: `wTotalLength` is clamped to what was actually
/// asked for, and the scratch page is four times this.
const MAX_CONFIG_DESC: usize = 256;

/// Result of parsing a USB device's configuration descriptor for HID interfaces.
struct HidInterfaceInfo {
    protocol: HidType,
    iface_num: u8,
    ep_addr: u8,
    ep_max_packet: u16,
    ep_interval: u8,
}

/// What one configuration descriptor offered that this driver can drive.
enum Function {
    Hid(HidInterfaceInfo),
    Msc(MscInterface),
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

/// A little-endian 16-bit field at `at`, or 0 past the end. Descriptors are
/// byte-aligned in the wire format and land wherever the previous descriptor's
/// length put them, so the packed-struct reads this replaces were unaligned as
/// well as unbounded.
fn le16(buf: &[u8], at: usize) -> u16 {
    let lo = buf.get(at).copied().unwrap_or(0) as u16;
    let hi = buf.get(at + 1).copied().unwrap_or(0) as u16;
    lo | (hi << 8)
}

/// Walk a configuration descriptor for the first interface this driver can
/// bind, returning it with its configuration value.
///
/// Every field read here is device-supplied, including the lengths that decide
/// where the next descriptor starts — so the walk is bounded by the buffer, a
/// zero length terminates it rather than looping forever, and every field is
/// read through `get`. Mass storage wins a tie with HID because a device
/// offering a disk is a disk; nothing in this tree offers both.
fn parse_config(buf: &[u8]) -> Option<(u8, Function)> {
    let total_len = (le16(buf, 2) as usize).min(buf.len());
    let config_val = *buf.get(5)?;

    let mut hid: Option<HidInterfaceInfo> = None;
    let mut msc: Option<MscInterface> = None;
    // Which interface the endpoint descriptors that follow belong to.
    let mut current: Option<Function> = None;
    // A SuperSpeed companion describes the endpoint immediately before it.
    let mut last_ep_in: Option<bool> = None;

    let mut offset = 0usize;
    while offset + 2 <= total_len {
        let desc_len = buf[offset] as usize;
        let desc_type = buf[offset + 1];
        if desc_len == 0 {
            break;
        }
        let desc = match buf.get(offset..(offset + desc_len).min(total_len)) {
            Some(d) => d,
            None => break,
        };

        match desc_type {
            // Interface
            4 if desc.len() >= 9 => {
                if let Some(done) = current.take() {
                    match done {
                        Function::Hid(h) if hid.is_none() => hid = Some(h),
                        Function::Msc(m) if msc.is_none() => msc = Some(m),
                        _ => {}
                    }
                }
                let (class, sub, proto) = (desc[5], desc[6], desc[7]);
                current = if class == 0x08 && sub == 0x06 && proto == 0x50 {
                    Some(Function::Msc(MscInterface {
                        iface_num: desc[2],
                        in_ep: 0,
                        in_max_packet: 0,
                        in_max_burst: 0,
                        out_ep: 0,
                        out_max_packet: 0,
                        out_max_burst: 0,
                    }))
                } else if class == 3 {
                    let protocol = match (sub, proto) {
                        (1, 1) => Some(HidType::Keyboard),
                        (1, 2) => Some(HidType::Mouse),
                        (0, _) => Some(HidType::Tablet),
                        _ => None,
                    };
                    protocol.map(|protocol| {
                        Function::Hid(HidInterfaceInfo {
                            protocol,
                            iface_num: desc[2],
                            ep_addr: 0,
                            ep_max_packet: 0,
                            ep_interval: 0,
                        })
                    })
                } else {
                    None
                };
            }
            // Endpoint
            5 if desc.len() >= 7 => {
                let addr = desc[2];
                let transfer = desc[3] & 0x3;
                let max_packet = le16(desc, 4);
                last_ep_in = None;
                match &mut current {
                    Some(Function::Hid(h)) if addr & 0x80 != 0 && h.ep_addr == 0 => {
                        h.ep_addr = addr;
                        h.ep_max_packet = max_packet;
                        h.ep_interval = desc[6];
                    }
                    // Bulk only: a mass-storage interface's interrupt endpoint
                    // belongs to CBI, which this driver does not speak.
                    Some(Function::Msc(m)) if transfer == 2 => {
                        if addr & 0x80 != 0 && m.in_ep == 0 {
                            m.in_ep = addr;
                            m.in_max_packet = max_packet;
                            last_ep_in = Some(true);
                        } else if addr & 0x80 == 0 && m.out_ep == 0 {
                            m.out_ep = addr;
                            m.out_max_packet = max_packet;
                            last_ep_in = Some(false);
                        }
                    }
                    _ => {}
                }
            }
            // SuperSpeed Endpoint Companion, which is where a SuperSpeed
            // device states the burst size of the endpoint just above it.
            // Zero is legal and means one packet per burst, so a device that
            // omits this costs throughput and nothing else.
            0x30 if desc.len() >= 3 => {
                if let (Some(Function::Msc(m)), Some(is_in)) = (&mut current, last_ep_in) {
                    if is_in {
                        m.in_max_burst = desc[2];
                    } else {
                        m.out_max_burst = desc[2];
                    }
                }
            }
            _ => {}
        }
        offset += desc_len;
    }
    if let Some(done) = current {
        match done {
            Function::Hid(h) if hid.is_none() => hid = Some(h),
            Function::Msc(m) if msc.is_none() => msc = Some(m),
            _ => {}
        }
    }

    if let Some(m) = msc {
        if m.in_ep != 0 && m.out_ep != 0 {
            return Some((config_val, Function::Msc(m)));
        }
        log!("xHCI: mass-storage interface {} has no bulk pair, skipping", m.iface_num);
    }
    let h = hid?;
    (h.ep_addr != 0).then(|| (config_val, Function::Hid(h)))
}

/// Initialize and configure one USB device on a port.
pub fn init_device(ctrl: &mut XhciController, op_base: &Mmio, port_idx: u8) {
    let portsc_off = OP_PORT_BASE + port_idx as u64 * PORT_REG_SIZE;
    let portsc = op_base.read_u32(portsc_off);
    op_base.write_u32(portsc_off, (portsc & !PORTSC_RW1C) | PORTSC_PR);

    // A port that asserts CCS and then never asserts PRC — a device pulled
    // between the scan and the reset, a marginal cable, a reset the controller
    // will not run — costs that port and not the boot. This spin had no
    // deadline, on the boot CPU, before the scheduler exists and with nothing
    // logged.
    if !super::settles(|| super::PORT_ANSWERS && op_base.read_u32(portsc_off) & PORTSC_PRC != 0) {
        log!("xHCI: port {} never finished its reset (PORTSC {:#010x}); skipping it",
            port_idx + 1, op_base.read_u32(portsc_off));
        return;
    }
    let portsc = op_base.read_u32(portsc_off);
    op_base.write_u32(portsc_off, (portsc & !PORTSC_RW1C) | PORTSC_PRC);

    let portsc = op_base.read_u32(portsc_off);
    if portsc & PORTSC_PED == 0 {
        log!("xHCI: port {} not enabled after reset", port_idx + 1);
        return;
    }
    let speed = ((portsc >> 10) & 0xF) as u8;
    log!("xHCI: port {} reset, speed={}", port_idx + 1, speed);

    let mut enable_slot = Trb::ZERO;
    enable_slot.control = TRB_ENABLE_SLOT;
    ctrl.submit_command(enable_slot);
    let slot_id = match ctrl.wait_command() {
        Some((CC_SUCCESS, slot_id)) => slot_id as u8,
        Some((code, _)) => {
            log!("xHCI: Enable Slot failed, code={}", code);
            return;
        }
        None => {
            log!("xHCI: Enable Slot timed out on port {}", port_idx + 1);
            return;
        }
    };
    // A slot id is the controller's answer, not the driver's, and CONFIG's
    // MaxSlotsEn is only advisory to a controller that chooses to ignore it —
    // QEMU's does. Nothing of the driver's is written for this device yet, so
    // there is nothing here to unwind; the controller's own Device Slot is
    // already allocated, and stays that way, because the driver issues no
    // Disable Slot on any path. That leak is filed, and this is its fourth
    // call site.
    let Some(block) = ctrl.layout.device(slot_id) else {
        log!("xHCI: slot {} is beyond the pool's {} device blocks, dropping port {}",
            slot_id, ctrl.layout.dev_blocks, port_idx + 1);
        return;
    };
    log!("xHCI: slot {} enabled (dma +{:#x})", slot_id, block);

    let dma = ctrl.dma();
    let mut ep0_ring = TrbRing::init(dma.subslice(block + DEV_EP0_RING, PAGE));
    let input_ctx = dma.subslice(OFF_INPUT_CTX, PAGE);
    let input_ctx_ptr = input_ctx.base();
    let input_ctx_phys = input_ctx.phys();
    unsafe { input_ctx.zero(); }

    ctrl.write_ctx32(input_ctx_ptr, 0, 1, 0x3); // Add Slot + EP0
    let slot_dw0 = ((speed as u32) << 20) | (1u32 << 27);
    ctrl.write_ctx32(input_ctx_ptr, 1, 0, slot_dw0);
    ctrl.write_ctx32(input_ctx_ptr, 1, 1, (port_idx as u32 + 1) << 16);

    let max_packet = max_packet_for_speed(speed);
    let ep0_dw1 = (3u32 << 1) | (4u32 << 3) | ((max_packet as u32) << 16);
    ctrl.write_ctx32(input_ctx_ptr, 2, 1, ep0_dw1);
    let ep0_dequeue = ep0_ring.dequeue();
    ctrl.write_ctx32(input_ctx_ptr, 2, 2, ep0_dequeue as u32);
    ctrl.write_ctx32(input_ctx_ptr, 2, 3, (ep0_dequeue >> 32) as u32);
    ctrl.write_ctx32(input_ctx_ptr, 2, 4, 8);

    let out_ctx = dma.subslice(block + DEV_OUT_CTX, PAGE / 2);
    unsafe { out_ctx.zero(); }
    unsafe {
        let dcbaa = dma.ptr_at(OFF_DCBAA) as *mut u64;
        write_volatile(dcbaa.add(slot_id as usize), out_ctx.phys());
    }

    let mut addr_dev = Trb::ZERO;
    addr_dev.param = input_ctx_phys;
    addr_dev.control = TRB_ADDRESS_DEVICE | ((slot_id as u32) << 24);
    if ctrl.run_command(addr_dev, "Address Device").is_none() {
        return;
    }
    log!("xHCI: device addressed");

    // Descriptor scratch, and shared for the same reason the input context is:
    // it is dead the moment this function returns. The report buffer below is
    // the one that is not, so it lives in the device's own block.
    let data_buf = dma.subslice(OFF_DATA_BUF, PAGE);
    let data_buf_ptr = data_buf.base();
    let data_buf_phys = data_buf.phys();
    unsafe { write_bytes(data_buf_ptr, 0, MAX_CONFIG_DESC); }
    let code = ctrl.control_transfer(
        slot_id, &mut ep0_ring, 0x80, 0x06, 0x0100, 0, Some(data_buf_phys), 18,
    );
    if !matches!(code, Some(CC_SUCCESS) | Some(CC_SHORT_PACKET)) {
        log!("xHCI: GET_DESCRIPTOR(Device) failed, code={:?}", code);
        return;
    }

    let descriptor = unsafe { core::slice::from_raw_parts(data_buf_ptr, MAX_CONFIG_DESC) };
    log!("xHCI: device class={:#x} vendor={:04x} product={:04x}",
        descriptor[4], le16(descriptor, 8), le16(descriptor, 10));

    unsafe { write_bytes(data_buf_ptr, 0, MAX_CONFIG_DESC); }
    let code = ctrl.control_transfer(
        slot_id, &mut ep0_ring, 0x80, 0x06, 0x0200, 0,
        Some(data_buf_phys), MAX_CONFIG_DESC as u16,
    );
    if !matches!(code, Some(CC_SUCCESS) | Some(CC_SHORT_PACKET)) {
        log!("xHCI: GET_DESCRIPTOR(Config) failed, code={:?}", code);
        return;
    }

    let (config_val, function) = match parse_config(descriptor) {
        Some(f) => f,
        None => {
            log!("xHCI: no HID boot interface found, skipping");
            return;
        }
    };

    let code = ctrl.control_transfer(
        slot_id, &mut ep0_ring, 0x00, 0x09, config_val as u16, 0, None, 0,
    );
    if code != Some(CC_SUCCESS) {
        log!("xHCI: SET_CONFIGURATION failed, code={:?}", code);
        return;
    }
    log!("xHCI: configuration set");

    let info = match function {
        Function::Msc(msc) => {
            log!("xHCI: mass storage iface={} in={:#x}/{} out={:#x}/{}",
                msc.iface_num, msc.in_ep, msc.in_max_packet, msc.out_ep, msc.out_max_packet);
            super::msc::bind(ctrl, ep0_ring, slot_id, speed, port_idx, &msc);
            return;
        }
        Function::Hid(info) => info,
    };

    let kind = match info.protocol {
        HidType::Keyboard => "keyboard",
        HidType::Mouse => "mouse",
        HidType::Tablet => "tablet",
    };
    let ep_num = info.ep_addr & 0x0F;
    let int_ep_dci = ep_num * 2 + 1;
    log!("xHCI: HID {} iface={} ep={:#x} max_pkt={} interval={} dci={}",
        kind, info.iface_num, info.ep_addr, info.ep_max_packet, info.ep_interval, int_ep_dci);

    // SET_PROTOCOL (boot protocol) — only for boot-interface devices
    if info.protocol != HidType::Tablet {
        let code = ctrl.control_transfer(
            slot_id, &mut ep0_ring, 0x21, 0x0B, 0, info.iface_num as u16, None, 0,
        );
        if code != Some(CC_SUCCESS) {
            log!("xHCI: SET_PROTOCOL failed, code={:?}", code);
        }
    }

    let report = dma.subslice(block + DEV_REPORT, 8);
    let report_phys = report.phys();
    let report_ptr = report.base();

    let int_ring = TrbRing::init(dma.subslice(block + DEV_INT_RING, PAGE));

    let input_ctx = dma.subslice(OFF_INPUT_CTX, PAGE);
    let input_ctx_ptr = input_ctx.base();
    let input_ctx_phys = input_ctx.phys();
    unsafe { input_ctx.zero(); }

    ctrl.write_ctx32(input_ctx_ptr, 0, 1, (1u32 << (int_ep_dci as u32)) | 1);

    let slot_dw0 = ((speed as u32) << 20) | ((int_ep_dci as u32) << 27);
    ctrl.write_ctx32(input_ctx_ptr, 1, 0, slot_dw0);
    ctrl.write_ctx32(input_ctx_ptr, 1, 1, (port_idx as u32 + 1) << 16);

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
    ctrl.write_ctx32(input_ctx_ptr, ep_ctx_index, 0, interval_val << 16);

    let ep_dw1 = (3u32 << 1) | (7u32 << 3) | ((info.ep_max_packet as u32) << 16);
    ctrl.write_ctx32(input_ctx_ptr, ep_ctx_index, 1, ep_dw1);

    let int_dequeue = int_ring.dequeue();
    ctrl.write_ctx32(input_ctx_ptr, ep_ctx_index, 2, int_dequeue as u32);
    ctrl.write_ctx32(input_ctx_ptr, ep_ctx_index, 3, (int_dequeue >> 32) as u32);
    ctrl.write_ctx32(input_ctx_ptr, ep_ctx_index, 4, 8);

    let mut config_ep = Trb::ZERO;
    config_ep.param = input_ctx_phys;
    config_ep.control = TRB_CONFIGURE_EP | ((slot_id as u32) << 24);
    if ctrl.run_command(config_ep, "Configure Endpoint").is_none() {
        return;
    }
    log!("xHCI: endpoint configured");

    let report_size = match info.protocol {
        HidType::Keyboard => 8,
        HidType::Mouse => 4,
        HidType::Tablet => 6,
    };
    let role = match info.protocol {
        HidType::Keyboard => HidRole::Keyboard,
        // A pointer with no entry in the button table cannot be bound: it
        // would have to share another device's, and then each report of one
        // publishes the other's buttons as released.
        HidType::Mouse | HidType::Tablet => match crate::mouse::PointerSource::claim() {
            Some(source) => HidRole::Pointer(source),
            None => {
                log!("xHCI: slot {} is past the pointers this machine can number, dropping it",
                    slot_id);
                return;
            }
        },
    };
    let mut dev = HidDevice {
        slot_id,
        int_ep_dci,
        int_ring,
        report_phys,
        report_ptr,
        report_size,
        role,
        prev_report: [0; 8],
    };

    dev.requeue(&ctrl.db_base);
    // The ring offset is in the line because two devices of one class landing
    // on one ring is invisible from every other angle: both still enumerate,
    // both still bind, and both still deliver until their TRBs interleave.
    log!("xHCI: USB {} ready on slot {}, int_ring +{:#x}", kind, slot_id, block + DEV_INT_RING);
    // The same argument one level up, and the only place the merge is visible:
    // two pointers on two controllers both have a slot 1, so a source derived
    // from the slot id would be one entry and each report would publish the
    // other device's buttons as released.
    if let HidRole::Pointer(source) = dev.role {
        log!("xHCI: pointer on slot {} merges as source {}", slot_id, source.id());
    }
    ctrl.devices.push(dev);
}

/// Scan all ports on the controller and initialize connected HID devices.
/// Enumeration is serial by construction, which is what lets the input
/// context, the EP0 ring and the descriptor buffer be one each. Serial does not
/// mean quiet: a device bound on an earlier port is armed and delivering while
/// a later port enumerates, so the event ring carries its completions too and
/// both waits demux by slot id rather than by TRB type alone.
pub fn scan_ports(ctrl: &mut XhciController, op_base: &Mmio, max_ports: u8) {
    for p in 0..max_ports {
        let portsc = op_base.read_u32(OP_PORT_BASE + p as u64 * PORT_REG_SIZE);
        if portsc & PORTSC_CCS != 0 {
            log!("xHCI: port {} connected, speed={}", p + 1, (portsc >> 10) & 0xF);
            init_device(ctrl, op_base, p);
        }
    }
}
