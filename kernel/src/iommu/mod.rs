//! The unit that decides what a device may reach.
//!
//! `specs/iommu-spec.md` is the design. This module is stage I1 of its §9 and
//! nothing more: the machine's units are inventoried and described, and no
//! register of one is ever written. Translation is I2, interrupt remapping I3,
//! domains and mapping I4, and refusing a machine with no usable unit is I5 —
//! sequenced last on purpose, because a refusal landed before the first
//! userspace driver has moved costs every machine and protects nothing.
//!
//! So this module *refuses nothing*. Every case §2.2 gives a refusal message
//! for is reported here as a line naming the register it decided on, and the
//! boot continues. The messages themselves are not written yet either: a line
//! saying "ToyOS requires one" on a kernel that boots happily without one is a
//! comment that lies about its own code.
//!
//! §3 requires that Intel's register layout not leak into the names above
//! `vtd/`. Everything in this file is stated in terms an ARM SMMU also
//! answers, so a second backend drops in without the seam moving; nothing here
//! says `Dmar`, `Sagaw` or `SourceId`.

pub mod vtd;

/// The address width a device's translations cover.
///
/// A closed enum rather than a number, so `39` cannot be passed where a page
/// table level count is wanted. VT-d's AGAW encoding and an SMMU's `T0SZ` are
/// both derived from it inside their own backends, and `specs/iommu-spec.md`
/// §5.3's IOVA base is derived from it in the portable half.
///
/// Two variants and not the three §3 sketches. §5.3 and §11.5 both rule 57-bit
/// out — a fifth level of page tables for an address space nothing here needs
/// — so a `Bits57` would be a variant with no producer and no consumer, which
/// is the untested arm §5.2 spends a section arguing against.
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
}

/// `bb:dd.f`, so a line naming a stream can be matched against the one
/// `pci::enumerate` printed for the same function.
impl core::fmt::Display for StreamId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:02x}:{:02x}.{}", self.0 >> 8, (self.0 >> 3) & 0x1f, self.0 & 0x7)
    }
}

/// Inventory this machine's units and describe each on one line.
///
/// Called from the boot phase that reads ACPI and enumerates PCI, before any
/// driver `init`: §2.1, because the unit has to be programmed before the first
/// device is told to do DMA. At this stage nothing is programmed, so the
/// placement is what the next stage needs rather than what this one does.
///
/// x86-64 has one backend and the kernel has one architecture; the dispatch
/// this line will become is the seam, not the code.
pub fn init(rsdp_addr: u64) {
    vtd::init(rsdp_addr);
}
