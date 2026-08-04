//! What memory type firmware gave a physical range.
//!
//! Read-only: these registers are firmware's, and the kernel programs none of
//! them. It reads them because they are half of the answer to what a mapping
//! costs. The other half is the PAT entry the page selects
//! ([`crate::arch::pat`]), and every mapping made with
//! [`CachePolicy::DeferToMtrr`](crate::mm::paging::CachePolicy) selects entry
//! 0 — WB, the entry that takes whatever the MTRR says (SDM Vol. 3A
//! Table 11-7). For those pages what is here is the whole of the answer.
//!
//! The exception is the one range that asks for [`effective_under_wc`], where
//! the MTRR is *outvoted* rather than consulted.

use crate::arch::cpu;

const IA32_MTRRCAP: u32 = 0xFE;
const IA32_MTRR_DEF_TYPE: u32 = 0x2FF;
const IA32_MTRR_PHYSBASE0: u32 = 0x200;
/// Bit 11 of `IA32_MTRR_DEF_TYPE`: variable and fixed MTRRs are consulted at
/// all. Cleared means the whole address space is UC, whatever the ranges say.
const DEF_TYPE_ENABLE: u64 = 1 << 11;
/// Bit 11 of an `IA32_MTRR_PHYSMASK`.
const PHYSMASK_VALID: u64 = 1 << 11;
/// Physical address bits of a PHYSBASE/PHYSMASK. 4 KiB-aligned, and the top is
/// bounded by the CPU's physical address width; masking to 52 bits is the
/// architectural ceiling and never narrower than a real one.
const PHYS_MASK: u64 = 0x000F_FFFF_FFFF_F000;

/// A memory type as the MTRRs encode it. Values are the architectural encoding,
/// which is what the MSRs hold.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MemoryType {
    Uncacheable,
    WriteCombining,
    WriteThrough,
    WriteProtected,
    WriteBack,
}

impl MemoryType {
    fn from_encoding(raw: u8) -> Option<Self> {
        match raw {
            0x00 => Some(Self::Uncacheable),
            0x01 => Some(Self::WriteCombining),
            0x04 => Some(Self::WriteThrough),
            0x05 => Some(Self::WriteProtected),
            0x06 => Some(Self::WriteBack),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Uncacheable => "UC",
            Self::WriteCombining => "WC",
            Self::WriteThrough => "WT",
            Self::WriteProtected => "WP",
            Self::WriteBack => "WB",
        }
    }
}

/// Why a range has no single answer. Reported rather than resolved: a range
/// that straddles two MTRRs is a question about firmware, and picking one of
/// the two here would turn it into a claim.
pub enum Unknown {
    /// A variable MTRR holds an encoding the architecture does not define.
    ReservedEncoding(u8),
    /// Overlapping MTRRs whose types the architecture leaves undefined.
    Conflicting,
    /// Part of the range is covered and part is not.
    PartiallyCovered,
}

pub enum Effective {
    Known(MemoryType),
    Unknown(Unknown),
    /// MTRRs are off, so the whole address space is UC by architecture.
    MtrrsDisabled,
}

impl Effective {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Known(t) => t.name(),
            Self::MtrrsDisabled => "UC (MTRRs disabled)",
            Self::Unknown(Unknown::ReservedEncoding(_)) => "unknown (reserved MTRR encoding)",
            Self::Unknown(Unknown::Conflicting) => "unknown (overlapping MTRRs disagree)",
            Self::Unknown(Unknown::PartiallyCovered) => "unknown (range only partly covered)",
        }
    }
}

/// The memory type of a page whose PAT entry holds WC, over a range whose MTRR
/// state is `mtrr`.
///
/// SDM Vol. 3A Table 11-7, "Effective Page-Level Memory Types for Pentium III
/// and More Recent Processor Families": every row whose PAT entry value is WC
/// gives WC, under all five MTRR types. The row this kernel stands on is
/// `UC | WC | WC` — an MTRR's UC does **not** win over a PAT entry's WC, which
/// is the one cell where the folk rule "strong UC beats every PAT type" is
/// wrong, and is why the scanout needs no range register of its own.
///
/// [`Effective::MtrrsDisabled`] answers through that same row: §11.11.2.1 makes
/// UC the type of all of physical memory when `MTRRdefType.E` is clear, and UC
/// is one of the five the table indexes.
///
/// `None` where [`range_type`] has no single answer, because a range with no
/// MTRR type has no row and so no effective type either.
pub fn effective_under_wc(mtrr: &Effective) -> Option<MemoryType> {
    match mtrr {
        Effective::Known(_) | Effective::MtrrsDisabled => Some(MemoryType::WriteCombining),
        Effective::Unknown(_) => None,
    }
}

/// The architecture's rule for two MTRRs over the same address: UC beats
/// anything, WT beats WB, and every other disagreement is undefined.
fn combine(a: MemoryType, b: MemoryType) -> Option<MemoryType> {
    use MemoryType::{Uncacheable, WriteBack, WriteThrough};
    match (a, b) {
        (x, y) if x == y => Some(x),
        (Uncacheable, _) | (_, Uncacheable) => Some(Uncacheable),
        (WriteThrough, WriteBack) | (WriteBack, WriteThrough) => Some(WriteThrough),
        _ => None,
    }
}

/// The memory type firmware gave `[base, base + size)`.
///
/// Fixed MTRRs are not consulted: they describe the first 1 MiB only, and no
/// caller of this is asking about that.
pub fn range_type(base: u64, size: u64) -> Effective {
    let def_type = cpu::rdmsr(IA32_MTRR_DEF_TYPE);
    if def_type & DEF_TYPE_ENABLE == 0 {
        return Effective::MtrrsDisabled;
    }
    let default = match MemoryType::from_encoding(def_type as u8) {
        Some(t) => t,
        None => return Effective::Unknown(Unknown::ReservedEncoding(def_type as u8)),
    };

    let end = base + size;
    let mut covering: Option<MemoryType> = None;
    for i in 0..(cpu::rdmsr(IA32_MTRRCAP) & 0xFF) as u32 {
        let mask = cpu::rdmsr(IA32_MTRR_PHYSBASE0 + i * 2 + 1);
        if mask & PHYSMASK_VALID == 0 {
            continue;
        }
        // A PHYSMASK's set bits are contiguous and high, so the region it
        // describes is the aligned block starting at PHYSBASE whose length is
        // the mask's lowest set bit.
        let phys_mask = mask & PHYS_MASK;
        let base_msr = cpu::rdmsr(IA32_MTRR_PHYSBASE0 + i * 2);
        let region_start = base_msr & phys_mask;
        let region_end = region_start + (1u64 << phys_mask.trailing_zeros());
        if region_end <= base || region_start >= end {
            continue;
        }
        if region_start > base || region_end < end {
            // The range asked about is not uniform under this MTRR, and saying
            // which half won would be inventing an answer.
            return Effective::Unknown(Unknown::PartiallyCovered);
        }
        let t = match MemoryType::from_encoding(base_msr as u8) {
            Some(t) => t,
            None => return Effective::Unknown(Unknown::ReservedEncoding(base_msr as u8)),
        };
        covering = Some(match covering {
            None => t,
            Some(prev) => match combine(prev, t) {
                Some(merged) => merged,
                None => return Effective::Unknown(Unknown::Conflicting),
            },
        });
    }
    Effective::Known(covering.unwrap_or(default))
}
