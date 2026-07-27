use crate::mm::Mmio;
use crate::log;

const VENDOR_ID: u64 = 0x00;
const DEVICE_ID: u64 = 0x02;
const COMMAND: u64 = 0x04;
const PROG_IF: u64 = 0x09;
const SUBCLASS: u64 = 0x0A;
const CLASS: u64 = 0x0B;
const HEADER_TYPE: u64 = 0x0E;
const BAR_BASE: u64 = 0x10;
const CAPABILITIES_PTR: u64 = 0x34;

const MULTI_FUNCTION: u8 = 0x80;
const INVALID_VENDOR: u16 = 0xFFFF;

pub struct Capability<'a> {
    device: &'a PciDevice,
    offset: u64,
}

impl Capability<'_> {
    pub fn id(&self) -> u8 {
        self.device.read_config_u8(self.offset)
    }

    pub fn read_u8(&self, field: u64) -> u8 {
        self.device.read_config_u8(self.offset + field)
    }

    pub fn read_u16(&self, field: u64) -> u16 {
        self.device.read_config_u16(self.offset + field)
    }

    pub fn read_u32(&self, field: u64) -> u32 {
        self.device.read_config_u32(self.offset + field)
    }

    pub fn write_u16(&self, field: u64, val: u16) {
        self.device.write_config_u16(self.offset + field, val)
    }
}

/// PCI device identified by ECAM base + Bus/Device/Function.
pub struct PciDevice {
    mmio: Mmio,
    pub bus: u8,
    pub dev: u8,
    pub func: u8,
}

impl PciDevice {
    fn new(ecam: &crate::mm::Mmio, bus: u8, dev: u8, func: u8) -> Self {
        let offset = ((bus as u64) << 20)
            | ((dev as u64) << 15)
            | ((func as u64) << 12);
        Self { mmio: ecam.subregion(offset, 4096), bus, dev, func }
    }

    pub fn vendor_id(&self) -> u16 {
        self.mmio.read_u16(VENDOR_ID)
    }

    pub fn device_id(&self) -> u16 {
        self.mmio.read_u16(DEVICE_ID)
    }

    pub fn read_config_u8(&self, offset: u64) -> u8 {
        self.mmio.read_u8(offset)
    }

    pub fn read_config_u16(&self, offset: u64) -> u16 {
        self.mmio.read_u16(offset)
    }

    pub fn read_config_u32(&self, offset: u64) -> u32 {
        self.mmio.read_u32(offset)
    }

    /// Read a Base Address Register by index (0-5).
    pub fn read_bar_64(&self, index: u8) -> u64 {
        let offset = BAR_BASE + index as u64 * 4;
        let low = self.mmio.read_u32(offset) as u64;
        let bar_type = (low >> 1) & 0x3;
        if bar_type == 2 {
            let high = self.mmio.read_u32(offset + 4) as u64;
            ((high << 32) | low) & !0xF
        } else {
            low & !0xF
        }
    }

    pub fn write_config_u16(&self, offset: u64, val: u16) {
        self.mmio.write_u16(offset, val)
    }

    /// Enable memory space access and bus mastering in PCI command register.
    pub fn enable_bus_master(&self) {
        let cmd = self.mmio.read_u16(COMMAND);
        self.mmio.write_u16(COMMAND, cmd | 0x06);
    }

    pub fn capabilities(&self) -> CapabilityIter<'_> {
        let first = self.mmio.read_u8(CAPABILITIES_PTR);
        CapabilityIter { device: self, next: first }
    }

    /// Find a PCI device by class, subclass, and optional prog_if.
    pub fn find(ecam: &crate::mm::Mmio, class: u8, subclass: u8, prog_if: Option<u8>) -> Option<Self> {
        Self::scan(ecam, |pci| pci.matches_class(class, subclass, prog_if))
    }

    /// Find a PCI device by vendor and device ID.
    pub fn find_by_id(ecam: &crate::mm::Mmio, vendor: u16, device: u16) -> Option<Self> {
        Self::scan(ecam, |pci| pci.vendor_id() == vendor && pci.device_id() == device)
    }

    fn scan(ecam: &crate::mm::Mmio, predicate: impl Fn(&PciDevice) -> bool) -> Option<Self> {
        for bus in 0..=255u16 {
            for dev in 0..32u8 {
                let pci = PciDevice::new(ecam, bus as u8, dev, 0);
                if pci.vendor_id() == INVALID_VENDOR { continue; }

                if predicate(&pci) {
                    return Some(pci);
                }

                if pci.mmio.read_u8(HEADER_TYPE) & MULTI_FUNCTION != 0 {
                    for func in 1..=7u8 {
                        let pci = PciDevice::new(ecam, bus as u8, dev, func);
                        if pci.vendor_id() == INVALID_VENDOR { continue; }
                        if predicate(&pci) {
                            return Some(pci);
                        }
                    }
                }
            }
        }
        None
    }

    fn matches_class(&self, class: u8, subclass: u8, prog_if: Option<u8>) -> bool {
        if self.mmio.read_u8(CLASS) != class { return false; }
        if self.mmio.read_u8(SUBCLASS) != subclass { return false; }
        match prog_if {
            Some(pi) => self.mmio.read_u8(PROG_IF) == pi,
            None => true,
        }
    }
}

pub struct CapabilityIter<'a> {
    device: &'a PciDevice,
    next: u8,
}

impl<'a> Iterator for CapabilityIter<'a> {
    type Item = Capability<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next == 0 {
            return None;
        }
        let offset = self.next as u64;
        self.next = self.device.read_config_u8(offset + 1);
        Some(Capability { device: self.device, offset })
    }
}

/// Enumerate all PCIe devices via ECAM and print them.
pub fn enumerate(ecam: &crate::mm::Mmio) {
    log!("PCI: Enumerating devices...");

    for bus in 0..=255u16 {
        for dev in 0..32u8 {
            let pci = PciDevice::new(ecam, bus as u8, dev, 0);
            if pci.vendor_id() == INVALID_VENDOR { continue; }

            print_device(&pci);

            if pci.read_config_u8(HEADER_TYPE) & MULTI_FUNCTION != 0 {
                for func in 1..=7u8 {
                    let pci = PciDevice::new(ecam, bus as u8, dev, func);
                    if pci.vendor_id() != INVALID_VENDOR {
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
        pci.read_config_u8(CLASS), pci.read_config_u8(SUBCLASS),
        pci.vendor_id(), pci.device_id(),
        pci.read_config_u8(PROG_IF)
    );
}
