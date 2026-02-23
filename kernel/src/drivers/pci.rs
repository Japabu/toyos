use super::mmio::Mmio;
use crate::log;

// PCI configuration space offsets
const VENDOR_ID: u64 = 0x00;
const DEVICE_ID: u64 = 0x02;
const COMMAND: u64 = 0x04;
const PROG_IF: u64 = 0x09;
const SUBCLASS: u64 = 0x0A;
const CLASS: u64 = 0x0B;
const HEADER_TYPE: u64 = 0x0E;
const BAR_BASE: u64 = 0x10;

const MULTI_FUNCTION: u8 = 0x80;
const INVALID_VENDOR: u16 = 0xFFFF;

/// PCI device identified by ECAM base + Bus/Device/Function.
pub struct PciDevice {
    mmio: Mmio,
    pub bus: u8,
    pub dev: u8,
    pub func: u8,
}

impl PciDevice {
    fn new(ecam_base: u64, bus: u8, dev: u8, func: u8) -> Self {
        let addr = ecam_base
            | ((bus as u64) << 20)
            | ((dev as u64) << 15)
            | ((func as u64) << 12);
        Self { mmio: Mmio::new(addr), bus, dev, func }
    }

    /// Read a 64-bit Base Address Register by index (0–5).
    pub fn read_bar_64(&self, index: u8) -> u64 {
        self.mmio.read_u64(BAR_BASE + index as u64 * 4) & !0xF
    }

    /// Enable memory space access and bus mastering in PCI command register.
    pub fn enable_bus_master(&self) {
        let cmd = self.mmio.read_u16(COMMAND);
        self.mmio.write_u16(COMMAND, cmd | 0x06);
    }

    /// Find a PCI device by class, subclass, and optional prog_if.
    pub fn find(ecam_base: u64, class: u8, subclass: u8, prog_if: Option<u8>) -> Option<Self> {
        for bus in 0..=255u16 {
            for dev in 0..32u8 {
                let pci = PciDevice::new(ecam_base, bus as u8, dev, 0);
                if pci.mmio.read_u16(VENDOR_ID) == INVALID_VENDOR { continue; }

                if pci.matches(class, subclass, prog_if) {
                    return Some(pci);
                }

                if pci.mmio.read_u8(HEADER_TYPE) & MULTI_FUNCTION != 0 {
                    for func in 1..=7u8 {
                        let pci = PciDevice::new(ecam_base, bus as u8, dev, func);
                        if pci.mmio.read_u16(VENDOR_ID) == INVALID_VENDOR { continue; }
                        if pci.matches(class, subclass, prog_if) {
                            return Some(pci);
                        }
                    }
                }
            }
        }
        None
    }

    fn matches(&self, class: u8, subclass: u8, prog_if: Option<u8>) -> bool {
        if self.mmio.read_u8(CLASS) != class { return false; }
        if self.mmio.read_u8(SUBCLASS) != subclass { return false; }
        match prog_if {
            Some(pi) => self.mmio.read_u8(PROG_IF) == pi,
            None => true,
        }
    }
}

/// Enumerate all PCIe devices via ECAM and print them.
pub fn enumerate(ecam_base: u64) {
    log!("PCI: Enumerating devices...");

    for bus in 0..=255u16 {
        for dev in 0..32u8 {
            let pci = PciDevice::new(ecam_base, bus as u8, dev, 0);
            if pci.mmio.read_u16(VENDOR_ID) == INVALID_VENDOR { continue; }

            print_device(&pci);

            if pci.mmio.read_u8(HEADER_TYPE) & MULTI_FUNCTION != 0 {
                for func in 1..=7u8 {
                    let pci = PciDevice::new(ecam_base, bus as u8, dev, func);
                    if pci.mmio.read_u16(VENDOR_ID) != INVALID_VENDOR {
                        print_device(&pci);
                    }
                }
            }
        }
    }

    log!("PCI: Enumeration complete.");
}

fn print_device(pci: &PciDevice) {
    log!(
        "  PCI {:02x}:{:02x}.{} [{:02x}{:02x}] vendor={:04x} device={:04x} prog_if={:02x}",
        pci.bus, pci.dev, pci.func,
        pci.mmio.read_u8(CLASS), pci.mmio.read_u8(SUBCLASS),
        pci.mmio.read_u16(VENDOR_ID), pci.mmio.read_u16(DEVICE_ID),
        pci.mmio.read_u8(PROG_IF)
    );
}
