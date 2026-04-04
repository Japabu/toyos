use core::mem::size_of;
use core::ptr::{read_volatile, write_volatile, write_bytes};

use crate::log;
use super::{Mmio, Trb, TrbRing, XhciController, dma, RING_SIZE};
use super::{OFF_DCBAA, OFF_INPUT_CTX, OFF_EP0_RING, OFF_DATA_BUF, OFF_OUT_CTX1, OFF_OUT_CTX2, OFF_OUT_CTX3, OFF_KB_INT_RING, OFF_MOUSE_INT_RING};
use super::{TRB_ENABLE_SLOT, TRB_ADDRESS_DEVICE, TRB_CONFIGURE_EP, TRB_LINK};
use super::{OP_PORT_BASE, PORT_REG_SIZE, PORTSC_CCS, PORTSC_PED, PORTSC_PR, PORTSC_PRC, PORTSC_RW1C};
use super::hid::{HidType, HidDevice};

// Standard USB descriptor structures (packed because they come from hardware)

#[repr(C, packed)]
struct UsbDeviceDescriptor {
    b_length: u8,
    b_descriptor_type: u8,
    bcd_usb: u16,
    b_device_class: u8,
    b_device_sub_class: u8,
    b_device_protocol: u8,
    b_max_packet_size0: u8,
    id_vendor: u16,
    id_product: u16,
    bcd_device: u16,
    i_manufacturer: u8,
    i_product: u8,
    i_serial_number: u8,
    b_num_configurations: u8,
}

#[repr(C, packed)]
struct UsbConfigDescriptor {
    b_length: u8,
    b_descriptor_type: u8,
    w_total_length: u16,
    b_num_interfaces: u8,
    b_configuration_value: u8,
    i_configuration: u8,
    bm_attributes: u8,
    b_max_power: u8,
}

#[repr(C, packed)]
struct UsbInterfaceDescriptor {
    b_length: u8,
    b_descriptor_type: u8,
    b_interface_number: u8,
    b_alternate_setting: u8,
    b_num_endpoints: u8,
    b_interface_class: u8,
    b_interface_sub_class: u8,
    b_interface_protocol: u8,
    i_interface: u8,
}

#[repr(C, packed)]
struct UsbEndpointDescriptor {
    b_length: u8,
    b_descriptor_type: u8,
    b_endpoint_address: u8,
    bm_attributes: u8,
    w_max_packet_size: u16,
    b_interval: u8,
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

fn max_packet_for_speed(speed: u8) -> u16 {
    match speed {
        2 => 8,    // Low Speed
        1 => 64,   // Full Speed
        3 => 64,   // High Speed
        4 => 512,  // Super Speed
        _ => 8,
    }
}

/// Map slot_id (1-based) to DMA offset for output context.
fn output_ctx_offset(slot_id: u8) -> usize {
    match slot_id {
        1 => OFF_OUT_CTX1,
        2 => OFF_OUT_CTX2,
        3 => OFF_OUT_CTX3,
        _ => panic!("xHCI: too many USB slots (max 3)"),
    }
}

/// Parse the configuration descriptor for a HID interface.
fn parse_hid_config(data_buf: *const u8) -> Option<HidInterfaceInfo> {
    unsafe {
        let buf = data_buf;
        let config = &*(buf as *const UsbConfigDescriptor);
        let total_len = (config.w_total_length as usize).min(256);
        let config_val = config.b_configuration_value;

        let mut found_protocol: Option<HidType> = None;
        let mut iface_num: u8 = 0;
        let mut ep_addr: u8 = 0;
        let mut ep_max_packet: u16 = 0;
        let mut ep_interval: u8 = 0;

        let mut offset = 0usize;
        while offset + 2 <= total_len {
            let desc_len = read_volatile(buf.add(offset)) as usize;
            let desc_type = read_volatile(buf.add(offset + 1));
            if desc_len == 0 { break; }

            match desc_type {
                4 if offset + size_of::<UsbInterfaceDescriptor>() <= total_len => {
                    let intf = &*(buf.add(offset) as *const UsbInterfaceDescriptor);
                    if intf.b_interface_class == 3 {
                        found_protocol = if intf.b_interface_sub_class == 1 {
                            // Boot protocol interface
                            match intf.b_interface_protocol {
                                1 => Some(HidType::Keyboard),
                                2 => Some(HidType::Mouse),
                                _ => None,
                            }
                        } else if intf.b_interface_sub_class == 0 {
                            // Non-boot HID device (e.g. USB tablet)
                            Some(HidType::Tablet)
                        } else {
                            None
                        };
                        if found_protocol.is_some() {
                            iface_num = intf.b_interface_number;
                        }
                    } else {
                        found_protocol = None;
                    }
                }
                5 if found_protocol.is_some() && offset + size_of::<UsbEndpointDescriptor>() <= total_len => {
                    let ep = &*(buf.add(offset) as *const UsbEndpointDescriptor);
                    if ep.b_endpoint_address & 0x80 != 0 && ep_addr == 0 {
                        ep_addr = ep.b_endpoint_address;
                        ep_max_packet = ep.w_max_packet_size;
                        ep_interval = ep.b_interval;
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
    }
}

/// Initialize and configure one USB device on a port.
pub fn init_device(ctrl: &mut XhciController, op_base: &Mmio, port_idx: u8) {
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
        return;
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
        return;
    }
    let slot_id = slot_id as u8;
    ctrl.active_slot = slot_id;
    log!("xHCI: slot {} enabled", slot_id);

    // Reset EP0 ring for this device
    ctrl.reset_ep0_ring();

    // Address Device
    let dma = dma();
    let input_ctx = dma.subslice(OFF_INPUT_CTX, 0x1000);
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
    let ep0_dequeue = dma.phys() + OFF_EP0_RING as u64 | 1;
    ctrl.write_ctx32(input_ctx_ptr, 2, 2, ep0_dequeue as u32);
    ctrl.write_ctx32(input_ctx_ptr, 2, 3, (ep0_dequeue >> 32) as u32);
    ctrl.write_ctx32(input_ctx_ptr, 2, 4, 8);

    let out_ctx = dma.subslice(output_ctx_offset(slot_id), 0x1000);
    unsafe { out_ctx.zero(); }
    unsafe {
        let dcbaa = dma.ptr_at(OFF_DCBAA) as *mut u64;
        write_volatile(dcbaa.add(slot_id as usize), out_ctx.phys());
    }

    let mut addr_dev = Trb::ZERO;
    addr_dev.param = input_ctx_phys;
    addr_dev.control = TRB_ADDRESS_DEVICE | ((slot_id as u32) << 24);
    ctrl.submit_command(addr_dev);
    let (code, _) = ctrl.wait_command();
    if code != 1 {
        log!("xHCI: Address Device failed, code={}", code);
        return;
    }
    log!("xHCI: device addressed");

    // GET_DESCRIPTOR (Device)
    let data_buf = dma.subslice(OFF_DATA_BUF, 0x1000);
    let data_buf_ptr = data_buf.base();
    let data_buf_phys = data_buf.phys();
    unsafe { write_bytes(data_buf_ptr, 0, 256); }
    let code = ctrl.control_transfer(0x80, 0x06, 0x0100, 0, Some(data_buf_phys), 18);
    if code != 1 && code != 13 {
        log!("xHCI: GET_DESCRIPTOR(Device) failed, code={}", code);
        return;
    }

    let (dev_class, vendor_id, product_id) = unsafe {
        let desc = &*(data_buf_ptr as *const UsbDeviceDescriptor);
        (desc.b_device_class, desc.id_vendor, desc.id_product)
    };
    log!("xHCI: device class={:#x} vendor={:04x} product={:04x}", dev_class, vendor_id, product_id);

    // GET_DESCRIPTOR (Configuration)
    unsafe { write_bytes(data_buf_ptr, 0, 256); }
    let code = ctrl.control_transfer(0x80, 0x06, 0x0200, 0, Some(data_buf_phys), 256);
    if code != 1 && code != 13 {
        log!("xHCI: GET_DESCRIPTOR(Config) failed, code={}", code);
        return;
    }

    let info = match parse_hid_config(data_buf_ptr) {
        Some(i) => i,
        None => {
            log!("xHCI: no HID boot interface found, skipping");
            return;
        }
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

    // SET_CONFIGURATION
    let code = ctrl.control_transfer(0x00, 0x09, info.config_val as u16, 0, None, 0);
    if code != 1 {
        log!("xHCI: SET_CONFIGURATION failed, code={}", code);
        return;
    }
    log!("xHCI: configuration set");

    // SET_PROTOCOL (boot protocol) — only for boot-interface devices
    if info.protocol != HidType::Tablet {
        let code = ctrl.control_transfer(0x21, 0x0B, 0, info.iface_num as u16, None, 0);
        if code != 1 {
            log!("xHCI: SET_PROTOCOL failed, code={}", code);
        }
    }

    // Choose interrupt ring and report buffer based on device type
    let (int_ring_off, report_buf_offset): (usize, usize) = match info.protocol {
        HidType::Keyboard => (OFF_KB_INT_RING, 512),
        HidType::Mouse | HidType::Tablet => (OFF_MOUSE_INT_RING, 1024),
    };
    let report_phys = data_buf.phys() + report_buf_offset as u64;
    let report_ptr = data_buf.ptr_at(report_buf_offset);

    // Set up interrupt ring link TRB
    let int_ring = dma.subslice(int_ring_off, 0x1000);
    unsafe { int_ring.zero(); }
    let mut int_link = Trb::ZERO;
    int_link.param = int_ring.phys();
    int_link.control = TRB_LINK | (1 << 1);
    unsafe { write_volatile((int_ring.base() as *mut Trb).add(RING_SIZE - 1), int_link); }

    // Configure Endpoint
    let input_ctx = dma.subslice(OFF_INPUT_CTX, 0x1000);
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

    let int_dequeue = int_ring.phys() | 1;
    ctrl.write_ctx32(input_ctx_ptr, ep_ctx_index, 2, int_dequeue as u32);
    ctrl.write_ctx32(input_ctx_ptr, ep_ctx_index, 3, (int_dequeue >> 32) as u32);
    ctrl.write_ctx32(input_ctx_ptr, ep_ctx_index, 4, 8);

    let mut config_ep = Trb::ZERO;
    config_ep.param = input_ctx_phys;
    config_ep.control = TRB_CONFIGURE_EP | ((slot_id as u32) << 24);
    ctrl.submit_command(config_ep);
    let (code, _) = ctrl.wait_command();
    if code != 1 {
        log!("xHCI: Configure Endpoint failed, code={}", code);
        return;
    }
    log!("xHCI: endpoint configured");

    // Store device and queue initial interrupt transfer
    let report_size = match info.protocol {
        HidType::Keyboard => 8,
        HidType::Mouse => 4,
        HidType::Tablet => 6,
    };
    let mut dev = HidDevice {
        slot_id,
        int_ep_dci,
        int_ring: TrbRing::new(int_ring),
        report_phys,
        report_ptr,
        report_size,
        hid_type: info.protocol,
    };

    dev.requeue(&ctrl.db_base);
    log!("xHCI: USB {} ready", kind);
    ctrl.devices.push(dev);
}

/// Scan all ports on the controller and initialize connected HID devices.
pub fn scan_ports(ctrl: &mut XhciController, op_base: &Mmio, max_ports: u8) {
    for p in 0..max_ports {
        let portsc = op_base.read_u32(OP_PORT_BASE + p as u64 * PORT_REG_SIZE);
        if portsc & PORTSC_CCS != 0 {
            log!("xHCI: port {} connected, speed={}", p + 1, (portsc >> 10) & 0xF);
            init_device(ctrl, op_base, p);
        }
    }
}
