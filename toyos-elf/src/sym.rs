//! `Elf64_Sym` tables, as a view over bytes.
//!
//! [`SymTab::get`] answers `None` past the end rather than reading a partial
//! record. The shape it replaces took a `&[u8]` and an index, sliced from
//! `index * 24` and read 24 bytes through a raw pointer: an index one short of
//! the end read past the buffer, and only the arithmetic at four separate call
//! sites kept it from happening.

use crate::read;

/// Bytes in one `Elf64_Sym`.
pub const ENTRY_SIZE: usize = 24;

pub const STB_GLOBAL: u8 = 1;
pub const STB_WEAK: u8 = 2;
pub const STT_TLS: u8 = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sym {
    pub name: u32,
    pub info: u8,
    pub shndx: u16,
    pub value: u64,
    pub size: u64,
}

impl Sym {
    /// `st_shndx != SHN_UNDEF`: this module defines the symbol rather than
    /// importing it.
    pub const fn is_defined(&self) -> bool {
        self.shndx != 0
    }

    pub const fn bind(&self) -> u8 {
        self.info >> 4
    }

    pub const fn kind(&self) -> u8 {
        self.info & 0xf
    }

    /// Whether another module may bind to this symbol.
    pub const fn is_exported(&self) -> bool {
        self.is_defined() && matches!(self.bind(), STB_GLOBAL | STB_WEAK)
    }
}

/// `.dynsym` (or `.symtab`) paired with the string table its names live in.
///
/// `count` is the entries the *bytes* hold, never a number the file declared:
/// `sh_size / sh_entsize` and a `.gnu.hash` chain walk are both file-chosen,
/// and a symbol index is bounded by what can actually be read.
#[derive(Clone, Copy, Debug)]
pub struct SymTab<'a> {
    syms: &'a [u8],
    strs: &'a [u8],
}

impl<'a> SymTab<'a> {
    pub const fn new(syms: &'a [u8], strs: &'a [u8]) -> SymTab<'a> {
        SymTab { syms, strs }
    }

    /// An empty table, for a module that declares no `DT_SYMTAB`.
    ///
    /// Every lookup answers `None` and every index is out of range, which is
    /// what "no symbol table" means — the shape it replaces was an
    /// `expect("no dynsym")` reached only because four separate callers each
    /// happened to check something else first.
    pub const fn empty() -> SymTab<'static> {
        SymTab {
            syms: &[],
            strs: &[],
        }
    }

    pub const fn count(&self) -> usize {
        self.syms.len() / ENTRY_SIZE
    }

    pub const fn strings(&self) -> &'a [u8] {
        self.strs
    }

    pub fn get(&self, i: usize) -> Option<Sym> {
        if i >= self.count() {
            return None;
        }
        Some(parse_at(self.syms, i * ENTRY_SIZE)?)
    }

    /// The `i`th symbol's name, or `""` for an index past the end or a
    /// `st_name` past the string table.
    pub fn name(&self, i: usize) -> &'a str {
        match self.get(i) {
            Some(sym) => read::cstr(self.strs, sym.name as usize),
            None => "",
        }
    }

    /// The first defined symbol with this name, skipping index 0 (always the
    /// null entry).
    pub fn find(&self, name: &str) -> Option<(usize, Sym)> {
        (1..self.count()).find_map(|i| {
            let sym = self.get(i)?;
            (sym.is_defined() && self.name(i) == name).then_some((i, sym))
        })
    }

    /// The first defined `STT_TLS` symbol with this name, and its offset within
    /// the defining module's TLS segment.
    pub fn find_tls(&self, name: &str) -> Option<u64> {
        (0..self.count()).find_map(|i| {
            let sym = self.get(i)?;
            (sym.is_defined() && sym.kind() == STT_TLS && self.name(i) == name).then_some(sym.value)
        })
    }

    /// Every index whose symbol is defined and named, in order.
    pub fn defined(&self) -> impl Iterator<Item = (usize, Sym)> + '_ {
        (1..self.count()).filter_map(|i| {
            let sym = self.get(i)?;
            sym.is_defined().then_some((i, sym))
        })
    }
}

/// One `Elf64_Sym` at a byte offset, for a caller holding a single record
/// rather than a table.
pub fn parse_at(data: &[u8], off: usize) -> Option<Sym> {
    if off >= data.len() {
        return None;
    }
    Some(Sym {
        name: read::u32_at(data, off)?,
        info: *data.get(off + 4)?,
        shndx: read::u16_at(data, off + 6)?,
        value: read::u64_at(data, off + 8)?,
        size: read::u64_at(data, off + 16)?,
    })
}
