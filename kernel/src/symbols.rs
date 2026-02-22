use alloc::string::String;
use alloc::vec::Vec;

use alloc::format;
use elf::ElfBytes;
use elf::endian::AnyEndian;
use elf::abi::STT_FUNC;
use crate::drivers::serial;

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
static mut PROCESS_SYMS: SymbolTable = SymbolTable::new();
// Kernel symbols (loaded once at boot, never cleared)
static mut KERNEL_SYMS: SymbolTable = SymbolTable::new();
// Memory range of loaded program (for backtrace address validation)
static mut PROG_BASE: u64 = 0;
static mut PROG_END: u64 = 0;
static mut STACK_BASE: u64 = 0;
static mut STACK_END: u64 = 0;

pub fn clear() {
    unsafe {
        (&raw mut PROCESS_SYMS).as_mut().unwrap().clear();
        (&raw mut PROG_BASE).write(0);
        (&raw mut PROG_END).write(0);
        (&raw mut STACK_BASE).write(0);
        (&raw mut STACK_END).write(0);
    }
}

/// Demangle a Rust symbol name (supports both legacy `_ZN...E` and v0 `_R...` mangling).
fn demangle(mangled: &str) -> String {
    format!("{:#}", rustc_demangle::demangle(mangled))
}

/// Look up a symbol by runtime address (checks both process and kernel symbols).
pub fn resolve(addr: u64) -> Option<(String, u64)> {
    let process = unsafe { &*(&raw const PROCESS_SYMS) };
    if let Some(result) = process.resolve(addr) {
        return Some(result);
    }
    let kernel = unsafe { &*(&raw const KERNEL_SYMS) };
    kernel.resolve(addr)
}

/// Check if an address is in user-accessible memory (program or stack).
pub fn is_valid_user_addr(addr: u64) -> bool {
    let prog_base = unsafe { *(&raw const PROG_BASE) };
    let prog_end = unsafe { *(&raw const PROG_END) };
    let stack_base = unsafe { *(&raw const STACK_BASE) };
    let stack_end = unsafe { *(&raw const STACK_END) };
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
    let table = unsafe { &mut *(&raw mut PROCESS_SYMS) };
    parse_symtab(data, base, table);
    serial::println(&format!("symbols: loaded {} process symbols", table.symbols.len()));
}

/// Load kernel symbols from raw ELF bytes. Called once at boot.
pub fn load_kernel(data: &[u8], base: u64) {
    let table = unsafe { &mut *(&raw mut KERNEL_SYMS) };
    parse_symtab(data, base, table);
    serial::println(&format!("symbols: loaded {} kernel symbols", table.symbols.len()));
}

/// Record the memory ranges of a loaded process for backtrace validation.
pub fn set_process_ranges(prog_base: u64, prog_end: u64, stack_base: u64, stack_end: u64) {
    unsafe {
        (&raw mut PROG_BASE).write(prog_base);
        (&raw mut PROG_END).write(prog_end);
        (&raw mut STACK_BASE).write(stack_base);
        (&raw mut STACK_END).write(stack_end);
    }
}
