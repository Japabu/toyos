use alloc::vec::Vec;

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

    /// Resolve an address without allocating. Returns the mangled symbol name.
    fn resolve(&self, addr: u64) -> Option<(&str, u64)> {
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
        Some((raw, offset))
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

    /// Create empty symbols with memory bounds (for crash addr validation).
    pub fn empty_with_bounds(
        prog_base: u64, prog_end: u64,
        stack_base: u64, stack_end: u64,
    ) -> Self {
        Self { table: SymbolTable::new(), prog_base, prog_end, stack_base, stack_end }
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

/// Resolve and log an address against kernel symbols without allocating.
/// Demangles directly to serial via fmt::Write. Safe to call from exception handlers.
pub fn resolve_kernel(addr: u64) -> Option<u64> {
    resolve_kernel_inner(addr, false)
}

/// Like `resolve_kernel`, but uses try_lock to avoid deadlock.
/// Safe to call from double fault / NMI handlers where locks may already be held.
pub fn resolve_kernel_nonblocking(addr: u64) -> Option<u64> {
    resolve_kernel_inner(addr, true)
}

fn resolve_kernel_inner(addr: u64, nonblocking: bool) -> Option<u64> {
    let guard = if nonblocking { KERNEL_SYMS.try_lock() } else { Some(KERNEL_SYMS.lock()) };
    let Some(guard) = guard else {
        crate::log!("    {:#x}  [symbols locked]", addr);
        return None;
    };
    if let Some((raw, offset)) = guard.resolve(addr) {
        // Demangle directly to serial via fmt::Write — no allocation.
        crate::log!("    {:#x}  {:#}+{:#x}", addr, rustc_demangle::demangle(raw), offset);
        Some(offset)
    } else {
        drop(guard);
        let kernel_base = if nonblocking {
            KERNEL_BASE.try_lock().map(|g| *g)
        } else {
            Some(*KERNEL_BASE.lock())
        };
        if let Some(kb) = kernel_base {
            if kb != 0 && addr >= kb {
                crate::log!("    {:#x}  [kernel+{:#x}]", addr, addr - kb);
            } else {
                crate::log!("    {:#x}", addr);
            }
        } else {
            crate::log!("    {:#x}", addr);
        }
        None
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
