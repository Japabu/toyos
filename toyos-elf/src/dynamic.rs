//! `PT_DYNAMIC`: the tags a dynamic linker acts on.
//!
//! Every tag is `Option`, never zero-means-absent. A `DT_SYMTAB` of 0 is a
//! symbol table at vaddr zero, which is a legal address in a `ET_DYN` image —
//! and the loader has to tell "the file did not say" from "the file said
//! zero", because those two get different treatment at every use site.

use crate::read;

/// A table named by a (location, size) pair of tags.
///
/// Present only when *both* tags are, and the size is non-zero. A `DT_RELA`
/// with no `DT_RELASZ` names a table of unknown length, which is not a table
/// anything can read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Table {
    pub vaddr: u64,
    pub size: u64,
}

pub const DT_NULL: i64 = 0;
pub const DT_NEEDED: i64 = 1;
pub const DT_PLTRELSZ: i64 = 2;
pub const DT_STRTAB: i64 = 5;
pub const DT_SYMTAB: i64 = 6;
pub const DT_RELA: i64 = 7;
pub const DT_RELASZ: i64 = 8;
pub const DT_STRSZ: i64 = 10;
pub const DT_INIT_ARRAY: i64 = 25;
pub const DT_JMPREL: i64 = 23;
pub const DT_INIT_ARRAYSZ: i64 = 27;
pub const DT_GNU_HASH: i64 = 0x6fff_fef5u32 as i32 as i64;

/// Bytes in one `Elf64_Dyn`.
pub const ENTRY_SIZE: usize = 16;

#[derive(Clone, Copy, Debug, Default)]
pub struct Dynamic {
    pub rela: Option<Table>,
    pub jmprel: Option<Table>,
    pub init_array: Option<Table>,
    pub strtab: Option<u64>,
    pub strsz: Option<u64>,
    pub symtab: Option<u64>,
    pub gnu_hash: Option<u64>,
}

impl Dynamic {
    /// Decode the tags this loader acts on, stopping at `DT_NULL` or at the
    /// end of the segment.
    pub fn parse(data: &[u8]) -> Dynamic {
        let mut rela_at = None;
        let mut rela_sz = None;
        let mut jmprel_at = None;
        let mut jmprel_sz = None;
        let mut init_at = None;
        let mut init_sz = None;
        let mut out = Dynamic::default();

        for (tag, val) in Entries::new(data) {
            match tag {
                DT_RELA => rela_at = Some(val),
                DT_RELASZ => rela_sz = Some(val),
                DT_JMPREL => jmprel_at = Some(val),
                DT_PLTRELSZ => jmprel_sz = Some(val),
                DT_INIT_ARRAY => init_at = Some(val),
                DT_INIT_ARRAYSZ => init_sz = Some(val),
                DT_STRTAB => out.strtab = Some(val),
                DT_STRSZ => out.strsz = Some(val),
                DT_SYMTAB => out.symtab = Some(val),
                DT_GNU_HASH => out.gnu_hash = Some(val),
                _ => {}
            }
        }

        out.rela = Table::from_tags(rela_at, rela_sz);
        out.jmprel = Table::from_tags(jmprel_at, jmprel_sz);
        out.init_array = Table::from_tags(init_at, init_sz);
        out
    }

    /// The `DT_STRTAB`/`DT_STRSZ` pair, when the file names both.
    pub fn strtab_table(&self) -> Option<Table> {
        Table::from_tags(self.strtab, self.strsz)
    }

    /// `DT_NEEDED` offsets into the string table, in the order they appear.
    ///
    /// An iterator rather than a collection: the count is one per entry of a
    /// table whose length the file chose, so anything that materialises it is
    /// an allocation sized by untrusted input.
    pub fn needed(data: &[u8]) -> impl Iterator<Item = u64> + '_ {
        Entries::new(data).filter_map(|(tag, val)| (tag == DT_NEEDED).then_some(val))
    }
}

impl Table {
    fn from_tags(vaddr: Option<u64>, size: Option<u64>) -> Option<Table> {
        match (vaddr, size) {
            (Some(vaddr), Some(size)) if size > 0 => Some(Table { vaddr, size }),
            _ => None,
        }
    }
}

/// `(d_tag, d_val)` pairs up to `DT_NULL`.
struct Entries<'a> {
    data: &'a [u8],
    off: usize,
}

impl<'a> Entries<'a> {
    fn new(data: &'a [u8]) -> Self {
        Entries { data, off: 0 }
    }
}

impl Iterator for Entries<'_> {
    type Item = (i64, u64);

    fn next(&mut self) -> Option<(i64, u64)> {
        let tag = read::i64_at(self.data, self.off)?;
        let val = read::u64_at(self.data, self.off + 8)?;
        if tag == DT_NULL {
            return None;
        }
        self.off += ENTRY_SIZE;
        Some((tag, val))
    }
}
