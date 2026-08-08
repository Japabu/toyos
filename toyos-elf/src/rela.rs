//! `Elf64_Rela` tables, as a view over bytes.
//!
//! A relocation is an instruction to write `width` bytes at a file-chosen
//! offset with a file-influenced value, so this is the trust boundary's
//! sharpest edge. [`RelocKind::write_width`] is the one table the validator and
//! the writers both read: a type missing from it is a type nobody patches, and
//! a type in it that no writer handles would be validated for a write that
//! never happens. Neither can drift, because there is one table.

use crate::read;

/// Bytes in one `Elf64_Rela`.
pub const ENTRY_SIZE: usize = 24;

/// The x86-64 relocations this loader knows about.
///
/// `Other` carries the raw type rather than dropping it, so a log line can name
/// what it skipped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelocKind {
    /// `R_X86_64_GLOB_DAT`
    GlobDat,
    /// `R_X86_64_JUMP_SLOT`
    JumpSlot,
    /// `R_X86_64_RELATIVE`
    Relative,
    /// `R_X86_64_DTPMOD64`
    DtpMod64,
    /// `R_X86_64_DTPOFF64`
    DtpOff64,
    /// `R_X86_64_TPOFF64`
    Tpoff64,
    /// `R_X86_64_TPOFF32`
    Tpoff32,
    Other(u32),
}

impl RelocKind {
    pub const fn from_raw(r_type: u32) -> RelocKind {
        match r_type {
            6 => RelocKind::GlobDat,
            7 => RelocKind::JumpSlot,
            8 => RelocKind::Relative,
            16 => RelocKind::DtpMod64,
            17 => RelocKind::DtpOff64,
            18 => RelocKind::Tpoff64,
            23 => RelocKind::Tpoff32,
            other => RelocKind::Other(other),
        }
    }

    /// How many bytes the loader writes for this type, or `None` for one it
    /// never writes.
    pub const fn write_width(self) -> Option<u64> {
        match self {
            RelocKind::GlobDat
            | RelocKind::JumpSlot
            | RelocKind::Relative
            | RelocKind::DtpMod64
            | RelocKind::DtpOff64
            | RelocKind::Tpoff64 => Some(8),
            RelocKind::Tpoff32 => Some(4),
            RelocKind::Other(_) => None,
        }
    }

    /// Whether resolving this type reads the symbol table.
    ///
    /// `Relative` is the one written type that does not, so it is also the one
    /// whose `r_sym` needs no bound.
    pub const fn needs_symbol(self) -> bool {
        !matches!(self, RelocKind::Relative | RelocKind::Other(_))
    }

    /// Whether this type binds a symbol's address into a GOT slot.
    pub const fn is_bind(self) -> bool {
        matches!(self, RelocKind::GlobDat | RelocKind::JumpSlot)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rela {
    pub offset: u64,
    pub sym: u32,
    pub kind: RelocKind,
    pub addend: i64,
}

/// A relocation table, addressed by entry rather than by byte.
///
/// Holds no ownership and reserves nothing: `DT_RELASZ` is a length the file
/// chose, and the only honest bound on it is the bytes the caller already has.
#[derive(Clone, Copy, Debug)]
pub struct RelaTable<'a> {
    data: &'a [u8],
}

impl<'a> RelaTable<'a> {
    pub const fn new(data: &'a [u8]) -> RelaTable<'a> {
        RelaTable { data }
    }

    /// Whole entries the bytes hold. A trailing partial entry is not an entry.
    pub const fn len(&self) -> usize {
        self.data.len() / ENTRY_SIZE
    }

    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, i: usize) -> Option<Rela> {
        if i >= self.len() {
            return None;
        }
        let off = i * ENTRY_SIZE;
        let info = read::u64_at(self.data, off + 8)?;
        Some(Rela {
            offset: read::u64_at(self.data, off)?,
            sym: (info >> 32) as u32,
            kind: RelocKind::from_raw(info as u32),
            addend: read::i64_at(self.data, off + 16)?,
        })
    }

    /// Takes `self` by value — the table is a `Copy` view, so the iterator
    /// borrows the bytes rather than the caller's handle on them.
    pub fn iter(self) -> impl Iterator<Item = Rela> + 'a {
        (0..self.len()).filter_map(move |i| self.get(i))
    }
}

/// How many entries of each kind a set of tables holds.
///
/// The loader reserves exactly from these instead of letting a `Vec` double:
/// `DT_RELASZ` and `DT_PLTRELSZ` are bounded separately, so two individually
/// acceptable tables sum to a collection no bound on either input can catch,
/// and growth-by-doubling then overshoots that sum as well.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RelaCounts {
    pub relative: usize,
    pub bind: usize,
    pub tpoff64: usize,
    pub tpoff32: usize,
    pub dtpmod64: usize,
    pub dtpoff64: usize,
}

impl RelaCounts {
    pub fn of(entries: impl Iterator<Item = Rela>) -> RelaCounts {
        let mut counts = RelaCounts::default();
        for rela in entries {
            let slot = match rela.kind {
                RelocKind::Relative => &mut counts.relative,
                RelocKind::GlobDat | RelocKind::JumpSlot => &mut counts.bind,
                RelocKind::Tpoff64 => &mut counts.tpoff64,
                RelocKind::Tpoff32 => &mut counts.tpoff32,
                RelocKind::DtpMod64 => &mut counts.dtpmod64,
                RelocKind::DtpOff64 => &mut counts.dtpoff64,
                RelocKind::Other(_) => continue,
            };
            *slot += 1;
        }
        counts
    }

    /// The largest of the counts a caller actually reserves for.
    ///
    /// Deliberately not "the largest count": a ceiling on a kind nothing
    /// stores refuses a file over a collection that does not exist, and a
    /// library is about 99.5 % `RELATIVE` — so a cache that bounded itself on
    /// `relative`, which it keeps none of, would refuse to cache every real
    /// library in the tree.
    pub fn max_of(&self, kinds: &[RelocKind]) -> usize {
        kinds.iter().map(|&k| self.count_of(k)).max().unwrap_or(0)
    }

    pub fn count_of(&self, kind: RelocKind) -> usize {
        match kind {
            RelocKind::Relative => self.relative,
            RelocKind::GlobDat | RelocKind::JumpSlot => self.bind,
            RelocKind::Tpoff64 => self.tpoff64,
            RelocKind::Tpoff32 => self.tpoff32,
            RelocKind::DtpMod64 => self.dtpmod64,
            RelocKind::DtpOff64 => self.dtpoff64,
            RelocKind::Other(_) => 0,
        }
    }
}

/// Why a relocation cannot be applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelocError {
    /// `r_offset + width` does not fit a `u64`.
    OffsetOverflows,
    /// The write would land outside the window the loader owns.
    OutsideWindow,
    /// `r_sym` names an entry past the end of `.dynsym`.
    SymbolPastTable,
}

impl RelocError {
    pub const fn as_str(self) -> &'static str {
        match self {
            RelocError::OffsetOverflows => "ELF: relocation r_offset + width overflows",
            RelocError::OutsideWindow => "ELF: relocation r_offset outside the writable image",
            RelocError::SymbolPastTable => "ELF: relocation r_sym past .dynsym",
        }
    }
}

impl core::fmt::Display for RelocError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Check every entry the loader will ever write against the window it may write
/// into and the symbol table it may resolve through.
///
/// Validated ahead of the first write, not as each one happens: a module that
/// is refused halfway through has already been modified, and a `DTPOFF64` with
/// `r_sym == 0` writes `r_addend` verbatim — so an unvalidated `r_offset` is an
/// arbitrary 8-byte write with a file-chosen value.
///
/// The window is the *writable* one rather than the whole image: once the
/// module is cached its read-only pages are shared between processes, and the
/// write lands in a private allocation covering only that window.
pub fn validate(
    entries: impl Iterator<Item = Rela>,
    window: (u64, u64),
    sym_count: usize,
) -> Result<(), RelocError> {
    let (lo, hi) = window;
    for rela in entries {
        let Some(width) = rela.kind.write_width() else {
            continue;
        };
        let end = rela
            .offset
            .checked_add(width)
            .ok_or(RelocError::OffsetOverflows)?;
        if rela.offset < lo || end > hi {
            return Err(RelocError::OutsideWindow);
        }
        if rela.kind.needs_symbol() && rela.sym as usize >= sym_count {
            return Err(RelocError::SymbolPastTable);
        }
    }
    Ok(())
}
