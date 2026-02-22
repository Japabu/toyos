use core::ptr::{read_volatile, write_volatile};
use crate::log;

/// PCI device identified by ECAM base + Bus/Device/Function.
pub struct PciDevice {
    ecam_base: u64,
    pub bus: u8,
    pub dev: u8,
    pub func: u8,
}

impl PciDevice {
    fn ecam_addr(&self, offset: u16) -> *mut u8 {
        let addr = self.ecam_base
            + ((self.bus as u64) << 20)
            | ((self.dev as u64) << 15)
            | ((self.func as u64) << 12)
            | (offset as u64);
        addr as *mut u8
    }

    pub fn read_u8(&self, offset: u16) -> u8 {
        unsafe { read_volatile(self.ecam_addr(offset)) }
    }

    pub fn read_u16(&self, offset: u16) -> u16 {
        unsafe { read_volatile(self.ecam_addr(offset) as *const u16) }
    }

    pub fn read_u32(&self, offset: u16) -> u32 {
        unsafe { read_volatile(self.ecam_addr(offset) as *const u32) }
    }

    pub fn write_u16(&self, offset: u16, val: u16) {
        unsafe { write_volatile(self.ecam_addr(offset) as *mut u16, val) }
    }

    /// Read 64-bit BAR0 (BAR0 at offset 0x10 + BAR1 at offset 0x14).
    pub fn bar0_64(&self) -> u64 {
        let bar0 = self.read_u32(0x10) as u64;
        let bar1 = self.read_u32(0x14) as u64;
        (bar0 & 0xFFFF_FFF0) | (bar1 << 32)
    }

    /// Enable memory space access and bus mastering in PCI command register.
    pub fn enable_bus_master(&self) {
        let cmd = self.read_u16(0x04);
        self.write_u16(0x04, cmd | 0x06);
    }

    /// Find a PCI device by class, subclass, and optional prog_if.
    pub fn find(ecam_base: u64, class: u8, subclass: u8, prog_if: Option<u8>) -> Option<Self> {
        for bus in 0..=255u16 {
            for dev in 0..32u8 {
                let pci = PciDevice { ecam_base, bus: bus as u8, dev, func: 0 };
                if pci.read_u16(0x00) == 0xFFFF { continue; }

                if pci.matches(class, subclass, prog_if) {
                    return Some(pci);
                }

                if pci.read_u8(0x0E) & 0x80 != 0 {
                    for func in 1..=7u8 {
                        let pci = PciDevice { ecam_base, bus: bus as u8, dev, func };
                        if pci.read_u16(0x00) == 0xFFFF { continue; }
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
        let c = self.read_u8(0x0B);
        let sc = self.read_u8(0x0A);
        if c != class || sc != subclass { return false; }
        match prog_if {
            Some(pi) => self.read_u8(0x09) == pi,
            None => true,
        }
    }
}

/// Enumerate all PCIe devices via ECAM and print them.
pub fn enumerate(ecam_base: u64) {
    log::println("PCI: Enumerating devices...");

    for bus in 0..=255u16 {
        for dev in 0..32u8 {
            let pci = PciDevice { ecam_base, bus: bus as u8, dev, func: 0 };
            if pci.read_u16(0x00) == 0xFFFF { continue; }

            print_device(&pci);

            if pci.read_u8(0x0E) & 0x80 != 0 {
                for func in 1..=7u8 {
                    let pci = PciDevice { ecam_base, bus: bus as u8, dev, func };
                    if pci.read_u16(0x00) != 0xFFFF {
                        print_device(&pci);
                    }
                }
            }
        }
    }

    log::println("PCI: Enumeration complete.");
}

fn print_device(pci: &PciDevice) {
    let vendor_id = pci.read_u16(0x00);
    let device_id = pci.read_u16(0x02);
    let class = pci.read_u8(0x0B);
    let subclass = pci.read_u8(0x0A);
    let prog_if = pci.read_u8(0x09);

    log!(
        "  PCI {:02x}:{:02x}.{} [{:02x}{:02x}] vendor={:04x} device={:04x} prog_if={:02x}",
        pci.bus, pci.dev, pci.func, class, subclass, vendor_id, device_id, prog_if
    );
}
