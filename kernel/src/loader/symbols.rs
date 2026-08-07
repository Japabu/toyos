//! The executable's exported symbols, for binding a library's `GLOB_DAT` and
//! `JUMP_SLOT` slots against.
//!
//! Nothing here owns a string. The maps borrow the tables the caller read, so
//! they die with the spawn that built them — the shape this replaces leaked its
//! `.strtab` with `Vec::leak` to produce `&'static str` keys, which every
//! spawn of a binary exporting nothing through `.dynsym` paid again.

use alloc::vec::Vec;
use hashbrown::HashMap;

use super::read_elf_table;
use crate::file_backing::FileBacking;
use crate::UserAddr;
use toyos_elf::section::{SectionTable, SHT_SYMTAB};
use toyos_elf::sym::SymTab;
use toyos_elf::Layout;

/// Every defined, named symbol in a table, at its runtime address.
pub fn map_from<'a>(symbols: &SymTab<'a>, base: UserAddr) -> HashMap<&'a str, UserAddr> {
    let mut map = HashMap::with_capacity(symbols.count());
    for (i, sym) in symbols.defined() {
        let name = symbols.name(i);
        if !name.is_empty() {
            map.insert(name, base + sym.value);
        }
    }
    map
}

/// `.symtab` and its `.strtab`, read whole.
///
/// The fallback for a PIE that exports nothing through `.dynsym` — which is
/// every binary linked without `--export-dynamic`. Both lengths are
/// file-declared and both tables are read whole, so past one kernel allocation
/// there is no map to build: dropping it degrades that binary's symbol
/// resolution and says so, where reading part of a symbol table would degrade
/// it silently.
pub fn read_symtab(backing: &dyn FileBacking, layout: &Layout) -> Option<(Vec<u8>, Vec<u8>)> {
    let table = layout.section_headers?;
    let shdrs = crate::process::read_file_range(backing, table.file_offset, table.byte_len());
    let (syms, strs) = SectionTable::new(&shdrs).symbols(SHT_SYMTAB)?;

    let (Some(sym_data), Some(str_data)) = (
        read_elf_table(backing, syms.offset, syms.size as usize),
        read_elf_table(backing, strs.offset, strs.size as usize),
    ) else {
        log!(
            "ELF: .symtab {} / .strtab {} exceed one kernel allocation, no symbol map",
            syms.size, strs.size
        );
        return None;
    };
    Some((sym_data, str_data))
}
