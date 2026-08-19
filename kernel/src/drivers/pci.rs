use alloc::vec::Vec;

use toyos_pci::{bar, msi, msix};

use crate::mm::Mmio;
use crate::mm::paging::CachePolicy;
use crate::log;

const VENDOR_ID: u64 = 0x00;
const DEVICE_ID: u64 = 0x02;
const COMMAND: u64 = 0x04;
const PROG_IF: u64 = 0x09;
const SUBCLASS: u64 = 0x0A;
const CLASS: u64 = 0x0B;
const HEADER_TYPE: u64 = 0x0E;
const CAPABILITIES_PTR: u64 = 0x34;

const MULTI_FUNCTION: u8 = 0x80;
const INVALID_VENDOR: u16 = 0xFFFF;

/// The one MSI-X table entry this kernel programs.
///
/// A device here raises one vector, so one entry carries it — and a virtio
/// device is told this number too, because the entry it points its queues at
/// has to be the entry [`PciDevice::enable_msix`] wrote.
pub const MSIX_ENTRY: u16 = 0;

/// The address a message-signalled interrupt is DMA'd to, in either form:
/// the LAPIC window, physical destination 0, fixed delivery, edge. Every
/// device in this kernel already targets it, so every device interrupt lands
/// on the boot CPU and is spread from there by `irq_ring` plus the scheduler
/// rather than by the interrupt controller. Written once because MSI and
/// MSI-X differ in where the address is configured and not in what it is.
const MSG_ADDR: u32 = 0xFEE0_0000;

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

    pub fn write_u32(&self, field: u64, val: u32) {
        self.device.write_config_u32(self.offset + field, val)
    }
}

/// PCI device identified by ECAM base + Bus/Device/Function.
#[derive(Clone, Copy)]
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

    /// The physical address Memory Space BAR `index` names, or why this
    /// register describes no memory.
    ///
    /// A `Result`, not a `u64`, because there are three answers a BAR can give
    /// that are not an address and the caller has to say what it does without
    /// one — `toyos_pci::bar` decodes them and this reads the registers the
    /// decode asks for, the second one only when the device said it is part of
    /// this BAR. An index past [`bar::MAX_INDEX`] is a kernel bug rather than a
    /// device's claim (every caller writes a literal, and `enable_msix`'s comes
    /// from `msix`, which refuses the reserved indicators first), so it is a
    /// fail-fast and not a refusal.
    pub fn memory_bar(&self, index: u8) -> Result<bar::Memory, bar::Unusable> {
        assert!(index <= bar::MAX_INDEX, "PCI: BAR {index} — a Type 0 header has six");
        let offset = bar::BASE + index as u64 * 4;
        match bar::decode(self.mmio.read_u32(offset))? {
            bar::Width::Narrow(memory) => Ok(memory),
            bar::Width::Wide(wide) => wide.with_high(self.mmio.read_u32(offset + 4)),
        }
    }

    pub fn write_config_u16(&self, offset: u64, val: u16) {
        self.mmio.write_u16(offset, val)
    }

    pub fn write_config_u32(&self, offset: u64, val: u32) {
        self.mmio.write_u32(offset, val)
    }

    /// Enable memory space access and bus mastering in PCI command register.
    pub fn enable_bus_master(&self) {
        let cmd = self.mmio.read_u16(COMMAND);
        self.mmio.write_u16(COMMAND, cmd | 0x06);
    }

    /// Point this function's [`MSIX_ENTRY`] at `vector` and enable it.
    ///
    /// `false` is "this function's MSI-X cannot be armed", which is either the
    /// absence of the capability or a table this kernel declines to believe —
    /// the log line names which. What to *do* about it is the driver's
    /// decision and differs: an xHC falls back to [`Self::enable_msi`], a
    /// virtio device has nothing to fall back to and refuses itself.
    pub fn enable_msix(&self, vector: u8) -> bool {
        let Some(cap) = self.capabilities().find(|c| c.id() == msix::CAP_ID) else {
            return false;
        };
        let control = cap.read_u16(msix::MESSAGE_CONTROL);
        let table = match msix::Msix::decode(control, cap.read_u32(msix::TABLE)) {
            Ok(table) => table,
            Err(why) => {
                log!("PCI {:02x}:{:02x}.{}: MSI-X not armed, {}",
                    self.bus, self.dev, self.func, why);
                return false;
            }
        };
        // The BAR the capability named, decoded rather than assumed to be
        // memory: `msix` refuses a reserved *indicator*, and this is the other
        // half — a device may name BAR 2 and BAR 2 may be an I/O BAR, which is
        // the one path in this kernel where a device-supplied index reaches a
        // BAR decode.
        let base = match self.memory_bar(table.bir()) {
            Ok(memory) => memory.address(),
            Err(why) => {
                log!("PCI {:02x}:{:02x}.{}: MSI-X not armed, its table names BAR {} and {}",
                    self.bus, self.dev, self.func, table.bir(), why);
                return false;
            }
        };
        let address = match table.table_address(base) {
            Ok(address) => address,
            Err(why) => {
                log!("PCI {:02x}:{:02x}.{}: MSI-X not armed, {}",
                    self.bus, self.dev, self.func, why);
                return false;
            }
        };

        let entry = address + MSIX_ENTRY as u64 * msix::ENTRY_BYTES;
        let table = crate::mm::paging::map_mmio(entry, 0x1000, CachePolicy::DeferToMtrr);

        table.write_u32(msix::ENTRY_ADDRESS_LO, MSG_ADDR);
        table.write_u32(msix::ENTRY_ADDRESS_HI, 0);
        table.write_u32(msix::ENTRY_DATA, vector as u32);
        table.write_u32(msix::ENTRY_VECTOR_CONTROL, msix::ENTRY_UNMASKED);

        cap.write_u16(msix::MESSAGE_CONTROL, msix::Msix::enabled(control));
        true
    }

    /// Point this function's single MSI message at `vector` and enable it.
    ///
    /// Not a lesser or older mechanism than MSI-X: the device performs the
    /// same LAPIC write, with the address and data configured in config space
    /// instead of in a table in a BAR. A PCIe function that omits MSI-X
    /// essentially always offers this one, which is the difference between a
    /// controller this kernel can be told about and one it cannot.
    pub fn enable_msi(&self, vector: u8) -> bool {
        let Some(cap) = self.capabilities().find(|c| c.id() == msi::CAP_ID) else {
            return false;
        };

        let control = cap.read_u16(msi::MESSAGE_CONTROL);
        let msi = msi::Msi::decode(control);
        cap.write_u32(msi.address_lo(), MSG_ADDR);
        if let Some(address_hi) = msi.address_hi() {
            cap.write_u32(address_hi, 0);
        }
        cap.write_u16(msi.data(), vector as u16);
        if let Some(mask) = msi.mask() {
            cap.write_u32(mask, 0);
        }
        cap.write_u16(msi::MESSAGE_CONTROL, msi::Msi::enabled(control));
        true
    }

    pub fn capabilities(&self) -> CapabilityIter<'_> {
        let first = self.mmio.read_u8(CAPABILITIES_PTR);
        CapabilityIter { device: self, next: first }
    }

    pub fn is_id(&self, vendor: u16, device: u16) -> bool {
        self.vendor_id() == vendor && self.device_id() == device
    }

    pub fn matches_class(&self, class: u8, subclass: u8, prog_if: Option<u8>) -> bool {
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

/// The most functions [`enumerate`] will hand back.
///
/// The address space allows 65536 of them — 256 buses of 32 devices of 8
/// functions — and which of those firmware leaves decoded is not the kernel's
/// choice, so the walk needs a ceiling that is not the address space's. The
/// T14 Gen 2 presents 30; QEMU's q35 with every profile's devices presents
/// fewer. A machine past this loses only the functions past it, and says so.
const MAX_DEVICES: usize = 256;

/// Every PCIe function ECAM decodes, in bus/device/function order.
///
/// One walk for the whole kernel, with each driver selecting out of the
/// result — and selecting *all* of what it can drive, not the first. A first
/// match is the wrong answer on the machine this targets: Tiger Lake puts an
/// xHCI in the Thunderbolt block at 00:0d.0 and the PCH's at 00:14.0, both
/// class 0c03 prog_if 30, and the laptop's keyboard hangs off the second one.
pub fn enumerate(ecam: &crate::mm::Mmio) -> Vec<PciDevice> {
    log!("PCI: Enumerating devices...");

    let mut found: Vec<PciDevice> = Vec::new();
    'scan: for bus in 0..=255u16 {
        for dev in 0..32u8 {
            let root = PciDevice::new(ecam, bus as u8, dev, 0);
            if root.vendor_id() == INVALID_VENDOR { continue; }

            let funcs = if root.read_config_u8(HEADER_TYPE) & MULTI_FUNCTION != 0 { 8 } else { 1 };
            for func in 0..funcs {
                let pci = PciDevice::new(ecam, bus as u8, dev, func);
                if pci.vendor_id() == INVALID_VENDOR { continue; }

                print_device(&pci);
                if found.len() == MAX_DEVICES {
                    log!("PCI: more than {} functions decoded; the rest are not enumerated",
                        MAX_DEVICES);
                    break 'scan;
                }
                found.push(pci);
            }
        }
    }

    log!("PCI: Enumeration complete, {} functions.", found.len());
    found
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
