use alloc::vec::Vec;

use elf::ElfBytes;
use elf::endian::AnyEndian;
use elf::abi::STT_FUNC;
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

/// Per-process symbol info. Points directly into the ELF in memory (initrd).
/// No copying, no allocation — just pointers to .symtab and .strtab.
pub struct ProcessSymbols {
    /// Raw .symtab section data in memory.
    symtab: *const u8,
    symtab_entries: usize,
    /// Raw .strtab section data in memory.
    strtab: *const u8,
    strtab_len: usize,
    /// ELF load base address.
    base: u64,
    prog_base: u64,
    prog_end: u64,
    stack_base: u64,
    stack_end: u64,
}

// Safety: the initrd is mapped for the kernel's entire lifetime.
unsafe impl Send for ProcessSymbols {}
unsafe impl Sync for ProcessSymbols {}

impl ProcessSymbols {
    pub fn empty() -> Self {
        Self {
            symtab: core::ptr::null(),
            symtab_entries: 0,
            strtab: core::ptr::null(),
            strtab_len: 0,
            base: 0,
            prog_base: 0,
            prog_end: 0,
            stack_base: 0,
            stack_end: 0,
        }
    }

    /// Create with memory bounds but no symbols (no section headers available).
    pub fn empty_with_bounds(
        prog_base: u64, prog_end: u64,
        stack_base: u64, stack_end: u64,
    ) -> Self {
        Self {
            symtab: core::ptr::null(),
            symtab_entries: 0,
            strtab: core::ptr::null(),
            strtab_len: 0,
            base: 0,
            prog_base, prog_end, stack_base, stack_end,
        }
    }

    /// Create from raw pointers to .symtab and .strtab already in memory.
    pub fn from_raw(
        symtab: *const u8, symtab_entries: usize,
        strtab: *const u8, strtab_len: usize,
        base: u64,
        prog_base: u64, prog_end: u64,
        stack_base: u64, stack_end: u64,
    ) -> Self {
        Self {
            symtab, symtab_entries,
            strtab, strtab_len,
            base,
            prog_base, prog_end, stack_base, stack_end,
        }
    }

    pub fn is_valid_user_addr(&self, addr: u64) -> bool {
        (addr >= self.prog_base && addr < self.prog_end)
            || (addr >= self.stack_base && addr < self.stack_end)
    }

    /// Resolve a user address to (mangled_name, offset).
    /// Scans the .symtab in-place — no allocation.
    pub fn resolve(&self, addr: u64) -> Option<(&str, u64)> {
        if self.symtab.is_null() || self.symtab_entries == 0 { return None; }

        const SYM_SIZE: usize = 24; // Elf64_Sym
        let mut best_addr = 0u64;
        let mut best_name_off = 0u32;
        let mut best_size = 0u64;

        for i in 0..self.symtab_entries {
            let entry = unsafe { self.symtab.add(i * SYM_SIZE) };
            let st_info = unsafe { *entry.add(4) };
            if (st_info & 0xf) != 2 { continue; } // STT_FUNC only
            let st_value = unsafe { u64::from_le_bytes(core::ptr::read_unaligned(entry.add(8) as *const [u8; 8])) };
            if st_value == 0 { continue; }
            let sym_addr = self.base + st_value;
            if sym_addr <= addr && sym_addr > best_addr {
                best_addr = sym_addr;
                best_name_off = unsafe { u32::from_le_bytes(core::ptr::read_unaligned(entry as *const [u8; 4])) };
                best_size = unsafe { u64::from_le_bytes(core::ptr::read_unaligned(entry.add(16) as *const [u8; 8])) };
            }
        }

        if best_addr == 0 { return None; }
        let offset = addr - best_addr;
        if best_size > 0 && offset >= best_size { return None; }

        let name = self.strtab_name(best_name_off as usize)?;
        Some((name, offset))
    }

    fn strtab_name(&self, off: usize) -> Option<&str> {
        if self.strtab.is_null() || off >= self.strtab_len { return None; }
        let start = unsafe { self.strtab.add(off) };
        let max_len = self.strtab_len - off;
        let len = (0..max_len).find(|&i| unsafe { *start.add(i) } == 0).unwrap_or(max_len);
        let bytes = unsafe { core::slice::from_raw_parts(start, len) };
        core::str::from_utf8(bytes).ok()
    }

    pub fn prog_base(&self) -> u64 {
        self.prog_base
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

/// Resolve and log a user address against a process's symbol table.
/// Returns true if the address could be identified.
/// Triggers lazy symbol loading on first call.
pub fn resolve_user(syms: &ProcessSymbols, addr: u64) -> bool {
    if let Some((name, offset)) = syms.resolve(addr) {
        log!("    {:#x}  {:#}+{:#x}", addr, rustc_demangle::demangle(name), offset);
        true
    } else if syms.is_valid_user_addr(addr) {
        let base_offset = addr.saturating_sub(syms.prog_base());
        log!("    {:#x}  [exe+{:#x}]", addr, base_offset);
        true
    } else {
        false
    }
}


/// Set the kernel base address for crash diagnostics.
pub fn set_kernel_base(base: u64) {
    *KERNEL_BASE.lock() = base;
}
