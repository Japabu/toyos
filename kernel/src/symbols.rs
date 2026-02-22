use alloc::string::String;
use alloc::vec::Vec;

use core::fmt::Write;
use alloc::format;
use elf::ElfBytes;
use elf::endian::AnyEndian;
use elf::abi::STT_FUNC;
use crate::drivers::serial;
use crate::sync::SyncCell;

struct Symbol {
    addr: u64,
    size: u64,
    name_start: u32, // offset into associated names vec
}

struct SymbolTable {
    symbols: Vec<Symbol>,
    names: Vec<u8>,
}

impl SymbolTable {
    const fn new() -> Self {
        Self { symbols: Vec::new(), names: Vec::new() }
    }

    fn clear(&mut self) {
        self.symbols.clear();
        self.names.clear();
    }

    fn resolve(&self, addr: u64) -> Option<(String, u64)> {
        if self.symbols.is_empty() { return None; }

        // Binary search for the last symbol with addr <= target
        let mut lo = 0usize;
        let mut hi = self.symbols.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.symbols[mid].addr <= addr {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }

        if lo == 0 { return None; }
        let sym = &self.symbols[lo - 1];
        let offset = addr - sym.addr;

        if sym.size > 0 && offset >= sym.size { return None; }

        let name_start = sym.name_start as usize;
        let name_end = self.names[name_start..].iter().position(|&b| b == 0)
            .map(|i| name_start + i)
            .unwrap_or(self.names.len());
        let raw = unsafe { core::str::from_utf8_unchecked(&self.names[name_start..name_end]) };
        Some((demangle(raw), offset))
    }
}

// Userland process symbols (cleared between process runs)
static PROCESS_SYMS: SyncCell<SymbolTable> = SyncCell::new(SymbolTable::new());
// Kernel symbols (loaded once at boot, never cleared)
static KERNEL_SYMS: SyncCell<SymbolTable> = SyncCell::new(SymbolTable::new());
// Memory range of loaded program (for backtrace address validation)
static PROG_BASE: SyncCell<u64> = SyncCell::new(0);
static PROG_END: SyncCell<u64> = SyncCell::new(0);
static STACK_BASE: SyncCell<u64> = SyncCell::new(0);
static STACK_END: SyncCell<u64> = SyncCell::new(0);

pub fn clear() {
    PROCESS_SYMS.get_mut().clear();
    *PROG_BASE.get_mut() = 0;
    *PROG_END.get_mut() = 0;
    *STACK_BASE.get_mut() = 0;
    *STACK_END.get_mut() = 0;
}

/// Demangle a Rust symbol name (supports both legacy `_ZN...E` and v0 `_R...` mangling).
fn demangle(mangled: &str) -> String {
    format!("{:#}", rustc_demangle::demangle(mangled))
}

/// Look up a symbol by runtime address (checks both process and kernel symbols).
pub fn resolve(addr: u64) -> Option<(String, u64)> {
    let process = PROCESS_SYMS.get();
    if let Some(result) = process.resolve(addr) {
        return Some(result);
    }
    let kernel = KERNEL_SYMS.get();
    kernel.resolve(addr)
}

/// Check if an address is in user-accessible memory (program or stack).
pub fn is_valid_user_addr(addr: u64) -> bool {
    let prog_base = *PROG_BASE.get();
    let prog_end = *PROG_END.get();
    let stack_base = *STACK_BASE.get();
    let stack_end = *STACK_END.get();
    (addr >= prog_base && addr < prog_end) || (addr >= stack_base && addr < stack_end)
}

/// Parse .symtab from raw ELF bytes into our SymbolTable.
fn parse_symtab(data: &[u8], base: u64, table: &mut SymbolTable) {
    table.clear();

    let elf = match ElfBytes::<AnyEndian>::minimal_parse(data) {
        Ok(e) => e,
        Err(_) => return,
    };

    let (symtab, strtab) = match elf.symbol_table() {
        Ok(Some(pair)) => pair,
        _ => return,
    };

    for sym in symtab.iter() {
        if sym.st_symtype() == STT_FUNC && sym.st_value != 0 {
            let name = match strtab.get(sym.st_name as usize) {
                Ok(n) => n,
                Err(_) => continue,
            };
            let name_start = table.names.len() as u32;
            table.names.extend_from_slice(name.as_bytes());
            table.names.push(0); // null terminator
            table.symbols.push(Symbol {
                addr: base + sym.st_value,
                size: sym.st_size,
                name_start,
            });
        }
    }

    table.symbols.sort_unstable_by_key(|s| s.addr);
}

/// Load userland process symbols from raw ELF bytes.
pub fn load_process(data: &[u8], base: u64) {
    let table = PROCESS_SYMS.get_mut();
    parse_symtab(data, base, table);
    let _ = writeln!(serial::SerialWriter, "symbols: loaded {} process symbols", table.symbols.len());
}

/// Load kernel symbols from raw ELF bytes. Called once at boot.
pub fn load_kernel(data: &[u8], base: u64) {
    let table = KERNEL_SYMS.get_mut();
    parse_symtab(data, base, table);
    let _ = writeln!(serial::SerialWriter, "symbols: loaded {} kernel symbols", table.symbols.len());
}

/// Record the memory ranges of a loaded process for backtrace validation.
pub fn set_process_ranges(prog_base: u64, prog_end: u64, stack_base: u64, stack_end: u64) {
    *PROG_BASE.get_mut() = prog_base;
    *PROG_END.get_mut() = prog_end;
    *STACK_BASE.get_mut() = stack_base;
    *STACK_END.get_mut() = stack_end;
}
