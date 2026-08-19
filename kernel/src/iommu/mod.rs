//! The unit that decides what a device may reach.
//!
//! **This module is the built half**: the machine's units are inventoried,
//! every enumerated PCI function is given a context entry naming one
//! identity-mapped domain, and translation is turned on. Interrupt remapping,
//! per-driver domains and mapping, and refusing a machine with no usable unit
//! are not built — `issues/kernel/the-iommu-stops-at-translation.md` is the
//! entry. The refusal is sequenced last on purpose, because one landed before
//! the first userspace driver has moved costs every machine and protects
//! nothing.
//!
//! So this module *refuses nothing*. Every condition that a machine with no
//! usable unit would one day be refused on — no unit declared by firmware or
//! none decodable, no interrupt remapping, no implemented address width, no
//! 2 MiB pages, no queued invalidation — is reported here as a line naming the
//! register it decided on, and the boot continues: a unit this kernel cannot
//! program is left off rather than made into a halt. The messages themselves
//! are not written yet either: a line saying "ToyOS requires one" on a kernel
//! that boots happily without one is a comment that lies about its own code.
//!
//! **Intel's register layout may not leak into the names above `vtd/`.**
//! Everything in this file is stated in terms an ARM SMMU also answers, so a
//! second backend drops in without the seam moving; nothing here says `Dmar`,
//! `Sagaw` or `SourceId`.
//!
//! What this stage deliberately does *not* add, so the per-driver domain work
//! does not have to unpick it: a `DomainId`, a `DmaPerm`, an `IommuError` and a
//! `trait Iommu`. There is one domain on this machine and one backend, so each
//! would be a type with a single value and a single implementor — a dead
//! abstraction, and the seam is not the code that would name it.

pub mod vtd;

/// The address width a device's translations cover.
///
/// A closed enum rather than a number, so `39` cannot be passed where a page
/// table level count is wanted. VT-d's AGAW encoding and an SMMU's `T0SZ` are
/// both derived from it inside their own backends, and the IOVA base — which
/// starts above the top of physical memory, so a device address is never a
/// valid physical address — is derived from it in the portable half.
///
/// Two variants and not three: 57-bit is out even on a unit that advertises it,
/// because it is a fifth level of page tables for an address space nothing here
/// needs — so a `Bits57` would be a variant with no producer and no consumer,
/// which is an arm no machine in reach would ever execute.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum AddressWidth {
    Bits39,
    Bits48,
}

impl AddressWidth {
    pub const fn bits(self) -> u8 {
        match self {
            Self::Bits39 => 39,
            Self::Bits48 => 48,
        }
    }
}

/// The unit's name for whoever issued a request: VT-d's 16-bit source-id, an
/// SMMU StreamID.
///
/// Wider than either, because the width is the backend's business and a
/// StreamID is 32 bits on an SMMU. Nothing outside this module tree can build
/// one, so the only values that exist are the ones a backend read off a
/// firmware table or off the bus.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct StreamId(u32);

impl StreamId {
    /// From a PCIe requester id.
    ///
    /// Named for where the number comes from rather than `new`, because a
    /// StreamID is not always a bus/device/function — an SMMU's comes out of
    /// IORT — and a constructor that is should say so.
    pub(in crate::iommu) const fn pci(bus: u8, device: u8, function: u8) -> Self {
        Self(((bus as u32) << 8) | ((device as u32) << 3) | function as u32)
    }

    /// Which bus this stream is on, and where in that bus's table it sits.
    ///
    /// Split rather than handed out whole because that is the shape a VT-d
    /// root/context pair indexes with, and an SMMU's stream table indexes with
    /// the whole id — so a backend that wants the other form still has one.
    pub(in crate::iommu) const fn bus(self) -> u8 {
        (self.0 >> 8) as u8
    }

    pub(in crate::iommu) const fn devfn(self) -> u8 {
        (self.0 & 0xFF) as u8
    }
}

/// An address a *device* uses. Never a physical address, never a virtual one.
/// Distinct from a physical address because confusing them is the whole bug
/// class this subsystem exists to close.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(transparent)]
pub struct Iova(u64);

impl Iova {
    /// The identity policy, and the single site that states it.
    ///
    /// §8.1 measured `ECAP.PT` clear on the only unit anyone here can boot, so
    /// §5.7's passthrough context type is unavailable and an identity-mapped
    /// translated domain is what every kernel-owned device gets. That makes
    /// each of its device addresses numerically equal to a physical one — a
    /// *policy*, not a fact about the two spaces. This constructor is where
    /// the policy lives, so the stage that stops identity-mapping deletes it
    /// and the compiler names every site that had assumed it.
    pub(in crate::iommu) const fn identity(phys: u64) -> Self {
        Self(phys)
    }

    pub(in crate::iommu) const fn raw(self) -> u64 {
        self.0
    }
}

/// `bb:dd.f`, so a line naming a stream can be matched against the one
/// `pci::enumerate` printed for the same function.
impl core::fmt::Display for StreamId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:02x}:{:02x}.{}", self.0 >> 8, (self.0 >> 3) & 0x1f, self.0 & 0x7)
    }
}

/// Inventory this machine's units, give every enumerated function a device
/// address space, and turn translation on.
///
/// Called from the boot phase that reads ACPI and enumerates PCI, before any
/// driver `init`: §2.1, because the unit has to be programmed before the first
/// device is told to do DMA. §5.1's rule that *every* function the walk
/// returned gets a context entry is why the device list is an argument —
/// enabling translation with an unenumerated device on the bus is how a
/// machine bricks its own boot disk.
///
/// x86-64 has one backend and the kernel has one architecture; the dispatch
/// this line will become is the seam, not the code.
pub fn init(rsdp_addr: u64, devices: &[crate::drivers::pci::PciDevice]) {
    vtd::init(rsdp_addr, devices);
}

/// The unit blocked a transaction and raised its fault event.
///
/// Reached from the IDT gate the unit's own `FEDATA` names. Not a device's
/// interrupt: it fires when a device has been told *no*, so what it reports is
/// a bug in whoever owns that device.
pub fn fault_interrupt() {
    vtd::fault::service();
}
