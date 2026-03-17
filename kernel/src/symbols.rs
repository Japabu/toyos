use alloc::sync::Arc;
use alloc::vec::Vec;

use elf::ElfBytes;
use elf::endian::AnyEndian;
use elf::section::SectionHeaderTable;
use elf::abi::{STT_FUNC, SHT_SYMTAB, SHT_STRTAB};
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

/// Info needed to lazily load symbols from disk on first crash.
struct LazySymbolSource {
    backing: Arc<dyn crate::file_backing::FileBacking>,
    sh_off: u64,
    sh_num: usize,
    sh_entsize: usize,
    base: u64,
}

/// Per-process symbol info: parsed function symbols + valid memory ranges.
/// Symbols are loaded lazily on first resolve (crash backtrace).
pub struct ProcessSymbols {
    table: SymbolTable,
    loaded: bool,
    lazy_source: Option<LazySymbolSource>,
    prog_base: u64,
    prog_end: u64,
    stack_base: u64,
    stack_end: u64,
}

impl ProcessSymbols {
    pub fn empty() -> Self {
        Self {
            table: SymbolTable::new(),
            loaded: true,
            lazy_source: None,
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
            table: SymbolTable::new(),
            loaded: true,
            lazy_source: None,
            prog_base, prog_end, stack_base, stack_end,
        }
    }

    /// Create lazy symbols — stores metadata for on-demand loading.
    pub fn lazy(
        backing: Arc<dyn crate::file_backing::FileBacking>,
        sh_off: u64, sh_num: usize, sh_entsize: usize,
        base: u64,
        prog_base: u64, prog_end: u64,
        stack_base: u64, stack_end: u64,
    ) -> Self {
        Self {
            table: SymbolTable::new(),
            loaded: false,
            lazy_source: Some(LazySymbolSource { backing, sh_off, sh_num, sh_entsize, base }),
            prog_base, prog_end, stack_base, stack_end,
        }
    }

    /// Ensure symbols are loaded (does disk I/O on first call).
    fn ensure_loaded(&mut self) {
        if self.loaded { return; }
        self.loaded = true;
        let Some(src) = self.lazy_source.take() else { return };
        load_from_source(&src, &mut self.table);
        log!("symbols: loaded {} user symbols (lazy)", self.table.symbols.len());
    }

    pub fn is_valid_user_addr(&self, addr: u64) -> bool {
        (addr >= self.prog_base && addr < self.prog_end)
            || (addr >= self.stack_base && addr < self.stack_end)
    }

    /// Resolve a user address to (mangled_name, offset).
    /// Triggers lazy loading on first call.
    pub fn resolve(&mut self, addr: u64) -> Option<(&str, u64)> {
        self.ensure_loaded();
        self.table.resolve(addr)
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

/// Load symbols from a lazy source (block map + section headers).
fn load_from_source(src: &LazySymbolSource, table: &mut SymbolTable) {
    use crate::process::read_file_range;

    let shdr_size = src.sh_num * src.sh_entsize;
    let shdr_data = read_file_range(src.backing.as_ref(), src.sh_off, shdr_size);

    let class = elf::file::Class::ELF64;
    let endian = AnyEndian::Little;
    let shdrs = SectionHeaderTable::new(endian, class, &shdr_data);

    // Find .symtab and its associated .strtab
    let mut symtab_shdr = None;
    let mut symtab_link = 0u32;
    for shdr in shdrs.iter() {
        if shdr.sh_type == SHT_SYMTAB {
            symtab_link = shdr.sh_link;
            symtab_shdr = Some(shdr);
            break;
        }
    }
    let Some(sym_shdr) = symtab_shdr else { return };

    let mut strtab_shdr = None;
    for (i, shdr) in shdrs.iter().enumerate() {
        if i as u32 == symtab_link && shdr.sh_type == SHT_STRTAB {
            strtab_shdr = Some(shdr);
            break;
        }
    }
    let Some(str_shdr) = strtab_shdr else { return };

    // Read .symtab and .strtab data from disk
    let sym_data = read_file_range(src.backing.as_ref(), sym_shdr.sh_offset, sym_shdr.sh_size as usize);
    let str_data = read_file_range(src.backing.as_ref(), str_shdr.sh_offset, str_shdr.sh_size as usize);

    let entry_size = if sym_shdr.sh_entsize > 0 { sym_shdr.sh_entsize as usize } else { 24 };
    let count = sym_data.len() / entry_size;

    for i in 0..count {
        let off = i * entry_size;
        if off + 24 > sym_data.len() { break; }
        let st_name = u32::from_le_bytes(sym_data[off..off + 4].try_into().unwrap()) as usize;
        let st_info = sym_data[off + 4];
        let st_value = u64::from_le_bytes(sym_data[off + 8..off + 16].try_into().unwrap());
        let st_size = u64::from_le_bytes(sym_data[off + 16..off + 24].try_into().unwrap());

        if (st_info & 0xf) != 2 || st_value == 0 { continue; }
        if st_name >= str_data.len() { continue; }

        let name_end = str_data[st_name..].iter().position(|&b| b == 0)
            .unwrap_or(str_data.len() - st_name);
        let name = &str_data[st_name..st_name + name_end];

        let name_start = table.names.len() as u32;
        table.names.extend_from_slice(name);
        table.names.push(0);
        table.symbols.push(Symbol {
            addr: src.base + st_value,
            size: st_size,
            name_start,
        });
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
pub fn resolve_user(syms: &mut ProcessSymbols, addr: u64) -> bool {
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
