use alloc::string::String;
use alloc::vec::Vec;
use alloc::format;

use elf::ElfBytes;
use elf::endian::AnyEndian;
use elf::abi::STT_FUNC;
use crate::log;
use crate::sync::Lock;

struct Symbol {
    addr: u64,
    size: u64,
    name_start: u32,
}

struct SymbolTable {
    symbols: Vec<Symbol>,
    names: Vec<u8>,
}

impl SymbolTable {
    const fn new() -> Self {
        Self { symbols: Vec::new(), names: Vec::new() }
    }

    fn resolve(&self, addr: u64) -> Option<(String, u64)> {
        if self.symbols.is_empty() { return None; }

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

/// Per-process symbol info: parsed function symbols + valid memory ranges.
pub struct ProcessSymbols {
    table: SymbolTable,
    prog_base: u64,
    prog_end: u64,
    stack_base: u64,
    stack_end: u64,
}

impl ProcessSymbols {
    pub fn empty() -> Self {
        Self {
            table: SymbolTable::new(),
            prog_base: 0,
            prog_end: 0,
            stack_base: 0,
            stack_end: 0,
        }
    }

    /// Parse symbols from ELF bytes and record memory ranges.
    pub fn parse(
        data: &[u8], base: u64,
        prog_base: u64, prog_end: u64,
        stack_base: u64, stack_end: u64,
    ) -> Self {
        let mut table = SymbolTable::new();
        parse_symtab(data, base, &mut table);
        Self { table, prog_base, prog_end, stack_base, stack_end }
    }

    pub fn resolve(&self, addr: u64) -> Option<(String, u64)> {
        self.table.resolve(addr)
    }

    pub fn is_valid_user_addr(&self, addr: u64) -> bool {
        (addr >= self.prog_base && addr < self.prog_end)
            || (addr >= self.stack_base && addr < self.stack_end)
    }

    pub fn symbol_count(&self) -> usize {
        self.table.symbols.len()
    }
}

// Kernel symbols (loaded once at boot, never cleared)
static KERNEL_SYMS: Lock<SymbolTable> = Lock::new(SymbolTable::new());
static KERNEL_BASE: Lock<u64> = Lock::new(0);

fn demangle(mangled: &str) -> String {
    format!("{:#}", rustc_demangle::demangle(mangled))
}

/// Resolve an address against kernel symbols only.
pub fn resolve_kernel(addr: u64) -> Option<(String, u64)> {
    KERNEL_SYMS.lock().resolve(addr)
}

/// Format an address with kernel symbol info if available.
pub fn format_kernel_addr(addr: u64) -> String {
    if let Some((name, offset)) = resolve_kernel(addr) {
        format!("{:#x}  {}+{:#x}", addr, name, offset)
    } else {
        let kernel_base = *KERNEL_BASE.lock();
        if kernel_base != 0 && addr >= kernel_base {
            format!("{:#x}  [kernel+{:#x}]", addr, addr - kernel_base)
        } else {
            format!("{:#x}", addr)
        }
    }
}

/// Format an address: try process symbols first, then kernel.
pub fn format_addr(addr: u64, proc_syms: &ProcessSymbols) -> String {
    if let Some((name, offset)) = proc_syms.resolve(addr) {
        format!("{:#x}  {}+{:#x}", addr, name, offset)
    } else if let Some((name, offset)) = resolve_kernel(addr) {
        format!("{:#x}  {}+{:#x}", addr, name, offset)
    } else {
        let kernel_base = *KERNEL_BASE.lock();
        if kernel_base != 0 && addr >= kernel_base {
            format!("{:#x}  [kernel+{:#x}]", addr, addr - kernel_base)
        } else {
            format!("{:#x}", addr)
        }
    }
}

/// Parse .symtab from raw ELF bytes into a SymbolTable.
fn parse_symtab(data: &[u8], base: u64, table: &mut SymbolTable) {
    table.symbols.clear();
    table.names.clear();

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
            table.names.push(0);
            table.symbols.push(Symbol {
                addr: base + sym.st_value,
                size: sym.st_size,
                name_start,
            });
        }
    }

    table.symbols.sort_unstable_by_key(|s| s.addr);
}

/// Load kernel symbols from raw ELF bytes. Called once at boot.
pub fn load_kernel(data: &[u8], base: u64) {
    let mut table = KERNEL_SYMS.lock();
    parse_symtab(data, base, &mut table);
    log!("symbols: loaded {} kernel symbols", table.symbols.len());
}

/// Set the kernel base address for crash diagnostics.
pub fn set_kernel_base(base: u64) {
    *KERNEL_BASE.lock() = base;
}
