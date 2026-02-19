use core::ptr::{read_volatile, write_volatile};
use crate::log;
use alloc::format;

/// Calculate the ECAM MMIO address for a given BDF + offset.
/// Formula: base + (bus << 20) | (device << 15) | (function << 12) | offset
fn ecam_addr(base: u64, bus: u8, dev: u8, func: u8, offset: u16) -> *mut u8 {
    let addr = base
        + (((bus as u64) << 20)
        | ((dev as u64) << 15)
        | ((func as u64) << 12)
        | (offset as u64));
    addr as *mut u8
}

pub fn ecam_read_u8(base: u64, bus: u8, dev: u8, func: u8, offset: u16) -> u8 {
    unsafe { read_volatile(ecam_addr(base, bus, dev, func, offset)) }
}

pub fn ecam_read_u16(base: u64, bus: u8, dev: u8, func: u8, offset: u16) -> u16 {
    unsafe { read_volatile(ecam_addr(base, bus, dev, func, offset) as *const u16) }
}

pub fn ecam_read_u32(base: u64, bus: u8, dev: u8, func: u8, offset: u16) -> u32 {
    unsafe { read_volatile(ecam_addr(base, bus, dev, func, offset) as *const u32) }
}

pub fn ecam_write_u16(base: u64, bus: u8, dev: u8, func: u8, offset: u16, val: u16) {
    unsafe { write_volatile(ecam_addr(base, bus, dev, func, offset) as *mut u16, val) }
}

/// Find a PCI device by class and subclass. Returns (bus, dev, func) or None.
pub fn find_device(ecam_base: u64, class: u8, subclass: u8) -> Option<(u8, u8, u8)> {
    for bus in 0..=255u16 {
        for dev in 0..32u8 {
            let vendor_id = ecam_read_u16(ecam_base, bus as u8, dev, 0, 0x00);
            if vendor_id == 0xFFFF { continue; }

            let c = ecam_read_u8(ecam_base, bus as u8, dev, 0, 0x0B);
            let sc = ecam_read_u8(ecam_base, bus as u8, dev, 0, 0x0A);
            if c == class && sc == subclass {
                return Some((bus as u8, dev, 0));
            }

            let header_type = ecam_read_u8(ecam_base, bus as u8, dev, 0, 0x0E);
            if header_type & 0x80 != 0 {
                for func in 1..=7u8 {
                    let vid = ecam_read_u16(ecam_base, bus as u8, dev, func, 0x00);
                    if vid == 0xFFFF { continue; }
                    let c = ecam_read_u8(ecam_base, bus as u8, dev, func, 0x0B);
                    let sc = ecam_read_u8(ecam_base, bus as u8, dev, func, 0x0A);
                    if c == class && sc == subclass {
                        return Some((bus as u8, dev, func));
                    }
                }
            }
        }
    }
    None
}

/// Read 64-bit BAR0 from a PCI device (BAR0 at offset 0x10 + BAR1 at offset 0x14).
pub fn read_bar0_64(ecam_base: u64, bus: u8, dev: u8, func: u8) -> u64 {
    let bar0 = ecam_read_u32(ecam_base, bus, dev, func, 0x10) as u64;
    let bar1 = ecam_read_u32(ecam_base, bus, dev, func, 0x14) as u64;
    (bar0 & 0xFFFF_FFF0) | (bar1 << 32)
}

/// Enable memory space access and bus mastering in PCI command register.
pub fn enable_bus_master(ecam_base: u64, bus: u8, dev: u8, func: u8) {
    let cmd = ecam_read_u16(ecam_base, bus, dev, func, 0x04);
    ecam_write_u16(ecam_base, bus, dev, func, 0x04, cmd | 0x06);
}

fn check_function(base: u64, bus: u8, dev: u8, func: u8) {
    let vendor_id = ecam_read_u16(base, bus, dev, func, 0x00);
    let device_id = ecam_read_u16(base, bus, dev, func, 0x02);
    let class = ecam_read_u8(base, bus, dev, func, 0x0B);
    let subclass = ecam_read_u8(base, bus, dev, func, 0x0A);
    let prog_if = ecam_read_u8(base, bus, dev, func, 0x09);

    log::println(&format!(
        "  PCI {:02x}:{:02x}.{} [{:02x}{:02x}] vendor={:04x} device={:04x} prog_if={:02x}",
        bus, dev, func, class, subclass, vendor_id, device_id, prog_if
    ));
}

/// Enumerate all PCIe devices via ECAM and print them to serial.
pub fn enumerate(ecam_base: u64) {
    log::println("PCI: Enumerating devices...");

    for bus in 0..=255u16 {
        for dev in 0..32u8 {
            let vendor_id = ecam_read_u16(ecam_base, bus as u8, dev, 0, 0x00);
            if vendor_id == 0xFFFF {
                continue;
            }

            check_function(ecam_base, bus as u8, dev, 0);

            let header_type = ecam_read_u8(ecam_base, bus as u8, dev, 0, 0x0E);
            if header_type & 0x80 != 0 {
                // Multi-function device, check functions 1-7
                for func in 1..=7u8 {
                    let vid = ecam_read_u16(ecam_base, bus as u8, dev, func, 0x00);
                    if vid != 0xFFFF {
                        check_function(ecam_base, bus as u8, dev, func);
                    }
                }
            }
        }
    }

    log::println("PCI: Enumeration complete.");
}
