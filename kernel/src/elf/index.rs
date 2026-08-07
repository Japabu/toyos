//! The executable's relocations, pre-computed at spawn and applied per page as
//! it is demand-faulted.
//!
//! The exe is not loaded into a kernel image the way a `.so` is — its pages
//! arrive one fault at a time — so every value it needs written is computed up
//! front and the fault handler only copies. Both collections here are reserved
//! exactly from a count, never grown: `DT_RELASZ` and `DT_PLTRELSZ` are bounded
//! separately and both feed one index, so two individually acceptable tables
//! sum to a collection no bound on either input can catch.

use alloc::vec::Vec;

use crate::mm::MAX_HEAP_ALLOC;
use toyos_elf::{RelaCounts, RelaTable, RelocKind};

/// Relocation entries the loader needs, grouped by what it does with them.
pub struct ParsedRelaEntries {
    /// `R_X86_64_RELATIVE`: (offset, addend).
    pub relative: Vec<(u64, i64)>,
    /// `R_X86_64_GLOB_DAT` and `R_X86_64_JUMP_SLOT`: (offset, symbol, addend).
    pub glob_dat: Vec<(u64, u32, i64)>,
    pub tpoff64: Vec<(u64, u32, i64)>,
    pub tpoff32: Vec<(u64, u32, i64)>,
}

/// Group both of the executable's relocation tables, or `None` when any one
/// group would not fit a single kernel allocation.
pub fn parse_rela_entries(rela_data: &[u8], jmprel_data: &[u8]) -> Option<ParsedRelaEntries> {
    let entries = || {
        RelaTable::new(rela_data)
            .iter()
            .chain(RelaTable::new(jmprel_data).iter())
    };
    let counts = RelaCounts::of(entries());
    // `relative` holds the narrowest record but the widest is what a
    // conservative ceiling has to assume, since any one group can be the whole
    // table.
    let widest = core::mem::size_of::<(u64, u32, i64)>();
    if counts.max().checked_mul(widest).is_none_or(|b| b > MAX_HEAP_ALLOC) {
        log!("ELF: {:?} will not fit one allocation", counts);
        return None;
    }
    let mut out = ParsedRelaEntries {
        relative: Vec::with_capacity(counts.relative),
        glob_dat: Vec::with_capacity(counts.bind),
        tpoff64: Vec::with_capacity(counts.tpoff64),
        tpoff32: Vec::with_capacity(counts.tpoff32),
    };
    for r in entries() {
        match r.kind {
            RelocKind::Relative => out.relative.push((r.offset, r.addend)),
            RelocKind::GlobDat | RelocKind::JumpSlot => {
                out.glob_dat.push((r.offset, r.sym, r.addend))
            }
            RelocKind::Tpoff64 => out.tpoff64.push((r.offset, r.sym, r.addend)),
            RelocKind::Tpoff32 => out.tpoff32.push((r.offset, r.sym, r.addend)),
            _ => {}
        }
    }
    Some(out)
}

/// Pre-computed writes, sorted by offset so a page's share of them is one
/// binary search away.
pub struct RelocationIndex {
    /// `RELATIVE` (base + addend), `GLOB_DAT` (a resolved address) and
    /// `TPOFF64` (a thread-pointer offset) are all 8 bytes.
    entries_u64: Vec<(u64, u64)>,
    /// `TPOFF32` patches a 4-byte immediate in place.
    entries_i32: Vec<(u64, i32)>,
}

impl RelocationIndex {
    /// Reserve exactly, or `None` when either collection would not fit one
    /// kernel allocation.
    ///
    /// The one place in the loader that needs a ceiling of its own rather than
    /// inheriting one: two separately-bounded tables feed this single index,
    /// and no bound on either input can catch their sum.
    pub fn with_capacity(u64_count: usize, i32_count: usize) -> Option<Self> {
        let fits =
            |n: usize, width: usize| n.checked_mul(width).is_some_and(|b| b <= MAX_HEAP_ALLOC);
        if !fits(u64_count, core::mem::size_of::<(u64, u64)>())
            || !fits(i32_count, core::mem::size_of::<(u64, i32)>())
        {
            return None;
        }
        Some(Self {
            entries_u64: Vec::with_capacity(u64_count),
            entries_i32: Vec::with_capacity(i32_count),
        })
    }

    pub fn add_u64(&mut self, offset: u64, value: u64) {
        self.entries_u64.push((offset, value));
    }

    pub fn add_i32(&mut self, offset: u64, value: i32) {
        self.entries_i32.push((offset, value));
    }

    /// Sort by offset. Must be called once every entry has been added.
    pub fn finalize(&mut self) {
        self.entries_u64.sort_unstable_by_key(|&(off, _)| off);
        self.entries_i32.sort_unstable_by_key(|&(off, _)| off);
    }

    /// Apply the writes that fall inside `[page_offset, page_offset + 4096)`,
    /// returning how many landed.
    ///
    /// A relocation straddling the page boundary is skipped rather than
    /// clipped: the loader validated it against the image, so the other half
    /// belongs to the next page and this is a page-at-a-time limitation, not a
    /// bounds failure.
    pub fn apply_to_page(&self, page_offset: u64, page_ptr: *mut u8) -> usize {
        let end_offset = page_offset + 4096;
        let mut count = 0usize;

        let start = self.entries_u64.partition_point(|&(off, _)| off < page_offset);
        for &(r_offset, value) in &self.entries_u64[start..] {
            if r_offset >= end_offset {
                break;
            }
            let within_page = (r_offset - page_offset) as usize;
            if within_page + 8 <= 4096 {
                unsafe {
                    core::ptr::write_unaligned(page_ptr.add(within_page) as *mut u64, value);
                }
                count += 1;
            }
        }

        let start = self.entries_i32.partition_point(|&(off, _)| off < page_offset);
        for &(r_offset, value) in &self.entries_i32[start..] {
            if r_offset >= end_offset {
                break;
            }
            let within_page = (r_offset - page_offset) as usize;
            if within_page + 4 <= 4096 {
                unsafe {
                    core::ptr::write_unaligned(page_ptr.add(within_page) as *mut i32, value);
                }
                count += 1;
            }
        }
        count
    }

    /// Whether any relocation falls inside `[page_offset, page_offset + 4096)`.
    pub fn has_relocs_in_page(&self, page_offset: u64) -> bool {
        let end_offset = page_offset + 4096;
        let start_u64 = self.entries_u64.partition_point(|&(off, _)| off < page_offset);
        if start_u64 < self.entries_u64.len() && self.entries_u64[start_u64].0 < end_offset {
            return true;
        }
        let start_i32 = self.entries_i32.partition_point(|&(off, _)| off < page_offset);
        start_i32 < self.entries_i32.len() && self.entries_i32[start_i32].0 < end_offset
    }

    pub fn len(&self) -> usize {
        self.entries_u64.len() + self.entries_i32.len()
    }
}
