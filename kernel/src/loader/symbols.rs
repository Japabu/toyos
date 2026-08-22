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
use crate::mm::pmm::Category;
use crate::process::PageAlloc;
use crate::symbols::SymbolTable;
use crate::UserAddr;
use toyos_elf::section::{SectionTable, SHT_SYMTAB};
use toyos_elf::sym::SymTab;
use toyos_elf::Layout;

/// Every defined, named symbol in `.dynsym`, at its runtime address.
///
/// No binding filter: `.dynsym` is the table of symbols the linker meant to
/// expose, so being in it and defined is the export.
pub fn dynamic_map<'a>(symbols: &SymTab<'a>, base: UserAddr) -> HashMap<&'a str, UserAddr> {
    map(symbols, base, |_| true)
}

/// The same over `.symtab`, which holds every symbol the binary has —
/// including its locals, which no other module may bind to.
pub fn static_map<'a>(symbols: &SymTab<'a>, base: UserAddr) -> HashMap<&'a str, UserAddr> {
    map(symbols, base, |s: &toyos_elf::Sym| s.is_exported())
}

fn map<'a>(
    symbols: &SymTab<'a>,
    base: UserAddr,
    keep: impl Fn(&toyos_elf::Sym) -> bool,
) -> HashMap<&'a str, UserAddr> {
    let mut map = HashMap::with_capacity(symbols.count());
    for (i, sym) in symbols.defined() {
        let name = symbols.name(i);
        if !name.is_empty() && keep(&sym) {
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

/// How much of one binary's symbol tables the kernel will hold so that its
/// backtraces name their frames.
///
/// Policy, and generous by design: the largest tables any binary in this tree
/// has are `bin/toyos-cc`'s 13,152,031 bytes, and `bin/sshd` is next at
/// 3,769,757 — so this is the next power of two above the real worst case. What
/// a caller sees when it is hit is a log line naming the binary and a process
/// whose backtraces are bare addresses; it is never a spawn failure, because a
/// program the kernel cannot narrate is still a program the machine can run.
pub const MAX_SYMBOL_BYTES: usize = 16 * 1024 * 1024;

/// The table a process's backtraces are named from, read off its own file.
///
/// The initrd used to be special here: [`SymbolTable`] took pointers straight
/// into it, through a `FileBacking::memory_ptr` no other backing had, so a
/// program run from a disk lost its symbol names and nothing said so. Reading
/// the file is what every other operating system does — Linux opens the on-disk
/// ELF, macOS a dSYM, Windows a PDB — and it is what lets the root filesystem be
/// a disk.
///
/// Into contiguous 2 MiB pages rather than a `Vec`, because
/// [`crate::mm::MAX_HEAP_ALLOC`] is under 2 MiB and two of the binaries this
/// tree ships have larger tables than that. The pages are the process's; the
/// resolve path still reads raw pointers and still allocates nothing.
// Eight arguments, for the reason `symbols::SymbolTable::from_pages` takes
// eight: four of them are the two address ranges a backtrace is checked
// against, and this is the call that hands them over.
#[allow(clippy::too_many_arguments)]
pub fn read_backtrace_table(
    backing: &dyn FileBacking,
    layout: &Layout,
    path: &str,
    base: u64,
    prog_base: u64, prog_end: u64,
    stack_base: u64, stack_end: u64,
) -> SymbolTable {
    let empty = || SymbolTable::empty_with_bounds(prog_base, prog_end, stack_base, stack_end);

    let Some(table) = layout.section_headers else { return empty() };
    let shdrs = crate::process::read_file_range(backing, table.file_offset, table.byte_len());
    let Some((syms, strs)) = SectionTable::new(&shdrs).symbols(SHT_SYMTAB) else {
        return empty();
    };

    // Both extents come off the file and are read into a buffer sized from
    // them, so both are bounded against the file rather than trusted. A section
    // that runs past EOF would otherwise be read as zeros — a table of null
    // entries, which resolves every address to nothing and says nothing about
    // why.
    let file_size = backing.file_size();
    let fits = |sh: &toyos_elf::section::SectionHeader| {
        sh.offset.checked_add(sh.size).is_some_and(|end| end <= file_size)
    };
    if syms.size == 0 || !fits(&syms) || !fits(&strs) {
        return empty();
    }

    let total = (syms.size + strs.size) as usize;
    if total > MAX_SYMBOL_BYTES {
        log!(
            "ELF: {}: {} bytes of symbol table past the {} byte bound — its backtraces will be \
             bare addresses",
            path, total, MAX_SYMBOL_BYTES
        );
        return empty();
    }

    let Some(pages) = PageAlloc::new(total, Category::Elf) else {
        log!("ELF: {}: no {} bytes for its symbol table", path, total);
        return empty();
    };
    // The two reads partition the allocation into exactly its two halves, and
    // `subslice` is what says so: `pages` was asked for `syms.size + strs.size`
    // bytes, and each window is bounded against what came back rather than
    // against that sum written out a second time.
    let dst = pages.window();
    if crate::elf::read_backing_into(backing, syms.offset, dst.subslice(0, syms.size as usize))
        .is_err()
        || crate::elf::read_backing_into(
            backing,
            strs.offset,
            dst.subslice(syms.size as usize, strs.size as usize),
        )
        .is_err()
    {
        log!("ELF: {}: its symbol table could not be read off the device", path);
        return empty();
    }

    SymbolTable::from_pages(
        pages,
        syms.size as usize,
        strs.size as usize,
        base,
        prog_base, prog_end, stack_base, stack_end,
    )
}
