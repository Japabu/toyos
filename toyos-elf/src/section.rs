//! The section header table, as a view over bytes.
//!
//! Sections are not needed to run a program — they are how the kernel finds
//! `.symtab` for a backtrace, and `.rela.dyn` in a file with no `PT_DYNAMIC`.
//! Nothing here refuses a file; a table that cannot be read simply names
//! nothing.

use crate::header::SECTION_HEADER_SIZE;
use crate::read;

pub const SHT_SYMTAB: u32 = 2;
pub const SHT_RELA: u32 = 4;
pub const SHT_DYNSYM: u32 = 11;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SectionHeader {
    pub name: u32,
    pub kind: u32,
    pub flags: u64,
    pub addr: u64,
    pub offset: u64,
    pub size: u64,
    /// `sh_link`: for `SHT_SYMTAB` and `SHT_DYNSYM`, the string table's index.
    pub link: u32,
    pub entry_size: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct SectionTable<'a> {
    data: &'a [u8],
}

impl<'a> SectionTable<'a> {
    pub const fn new(data: &'a [u8]) -> SectionTable<'a> {
        SectionTable { data }
    }

    /// Whole entries the bytes hold. A short read of the table leaves the
    /// entries it did cover usable, which is what the loader wants: a
    /// truncated table names fewer sections, not none.
    pub const fn len(&self) -> usize {
        self.data.len() / SECTION_HEADER_SIZE
    }

    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, i: usize) -> Option<SectionHeader> {
        if i >= self.len() {
            return None;
        }
        let off = i * SECTION_HEADER_SIZE;
        Some(SectionHeader {
            name: read::u32_at(self.data, off)?,
            kind: read::u32_at(self.data, off + 4)?,
            flags: read::u64_at(self.data, off + 8)?,
            addr: read::u64_at(self.data, off + 16)?,
            offset: read::u64_at(self.data, off + 24)?,
            size: read::u64_at(self.data, off + 32)?,
            link: read::u32_at(self.data, off + 40)?,
            entry_size: read::u64_at(self.data, off + 56)?,
        })
    }

    pub fn iter(&self) -> impl Iterator<Item = SectionHeader> + '_ {
        (0..self.len()).filter_map(|i| self.get(i))
    }

    /// The first section of this type.
    pub fn find(&self, kind: u32) -> Option<SectionHeader> {
        self.iter().find(|sh| sh.kind == kind)
    }

    /// A symbol table and the string table it points at, as (symbols,
    /// strings) file extents.
    ///
    /// `None` when there is no such section or its `sh_link` names no section
    /// in this table — a symbol table whose names are unreachable resolves
    /// every name to `""`, which is worse than having no map at all.
    pub fn symbols(&self, kind: u32) -> Option<(SectionHeader, SectionHeader)> {
        let syms = self.find(kind)?;
        let strs = self.get(syms.link as usize)?;
        Some((syms, strs))
    }

    /// The `.rela.dyn` section, for a file with no `PT_DYNAMIC`.
    ///
    /// Identified by shape rather than by name: `.shstrtab` would have to be
    /// located first to read section names, and `e_shstrndx` is not in this
    /// table. `first_entry` reads the section's first `Elf64_Rela` off the
    /// file, and only a table whose first entry is a `R_X86_64_RELATIVE`
    /// qualifies — a `SHT_RELA` of any other shape belongs to something else.
    pub fn rela_dyn(
        &self,
        first_entry: &mut dyn FnMut(u64) -> Option<crate::rela::Rela>,
    ) -> Option<(u64, u64)> {
        for sh in self.iter() {
            if sh.kind != SHT_RELA || sh.entry_size != crate::rela::ENTRY_SIZE as u64 || sh.size == 0
            {
                continue;
            }
            if first_entry(sh.offset).is_some_and(|r| r.kind == crate::rela::RelocKind::Relative) {
                return Some((sh.offset, sh.size));
            }
        }
        None
    }
}
