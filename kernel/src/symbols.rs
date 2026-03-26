use core::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

use alloc::boxed::Box;
use elf::ElfBytes;
use elf::endian::AnyEndian;

/// Zero-allocation symbol table. Points directly into ELF sections in memory.
/// Resolution is a linear scan over raw Elf64_Sym entries — O(n) but lock-free,
/// allocation-free, and safe to call from any context including panic/double-fault.
pub struct SymbolTable {
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

// Safety: the ELF data (initrd or kernel image) is mapped for the kernel's entire lifetime.
unsafe impl Send for SymbolTable {}
unsafe impl Sync for SymbolTable {}

impl SymbolTable {
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

    /// Parse an ELF in memory and create a SymbolTable pointing into its sections.
    /// No copying — only stores pointers into `data`.
    fn from_elf(data: &[u8], base: u64) -> Self {
        let elf = match ElfBytes::<AnyEndian>::minimal_parse(data) {
            Ok(e) => e,
            Err(_) => return Self::empty(),
        };

        // Find .symtab section header
        let shdrs = match elf.section_headers() {
            Some(s) => s,
            None => return Self::empty(),
        };

        const SHT_SYMTAB: u32 = 2;
        let mut symtab_shdr = None;
        for shdr in shdrs.iter() {
            if shdr.sh_type == SHT_SYMTAB {
                symtab_shdr = Some(shdr);
                break;
            }
        }
        let Some(shdr) = symtab_shdr else { return Self::empty() };

        let symtab_off = shdr.sh_offset as usize;
        let symtab_size = shdr.sh_size as usize;
        let entsize = if shdr.sh_entsize > 0 { shdr.sh_entsize as usize } else { 24 };
        let link = shdr.sh_link as usize;

        if symtab_off + symtab_size > data.len() { return Self::empty(); }

        // Get linked strtab
        let strtab_shdr = match shdrs.get(link) {
            Ok(s) => s,
            Err(_) => return Self::empty(),
        };
        let strtab_off = strtab_shdr.sh_offset as usize;
        let strtab_size = strtab_shdr.sh_size as usize;
        if strtab_off + strtab_size > data.len() { return Self::empty(); }

        let symtab_ptr = unsafe { data.as_ptr().add(symtab_off) };
        let strtab_ptr = unsafe { data.as_ptr().add(strtab_off) };
        let entries = symtab_size / entsize;

        Self {
            symtab: symtab_ptr,
            symtab_entries: entries,
            strtab: strtab_ptr,
            strtab_len: strtab_size,
            base,
            prog_base: 0,
            prog_end: 0,
            stack_base: 0,
            stack_end: 0,
        }
    }

    pub fn is_valid_user_addr(&self, addr: u64) -> bool {
        (addr >= self.prog_base && addr < self.prog_end)
            || (addr >= self.stack_base && addr < self.stack_end)
    }

    /// Resolve an address to (mangled_name, offset). Linear scan — no allocation, no lock.
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
            let st_size = unsafe { u64::from_le_bytes(core::ptr::read_unaligned(entry.add(16) as *const [u8; 8])) };
            if sym_addr <= addr && (sym_addr > best_addr
                || (sym_addr == best_addr && best_size == 0 && st_size > 0))
            {
                best_addr = sym_addr;
                best_name_off = unsafe { u32::from_le_bytes(core::ptr::read_unaligned(entry as *const [u8; 4])) };
                best_size = st_size;
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

// Kernel symbols — set once at boot, lock-free reads forever after.
static KERNEL_SYMS: AtomicPtr<SymbolTable> = AtomicPtr::new(core::ptr::null_mut());
static KERNEL_BASE: AtomicU64 = AtomicU64::new(0);

/// Set the kernel base address for crash diagnostics.
pub fn set_kernel_base(base: u64) {
    KERNEL_BASE.store(base, Ordering::Release);
}

/// Load kernel symbols from raw ELF bytes in the direct map. Called once at boot.
/// Stores pointers into the ELF data — the only allocation is the ~72-byte SymbolTable struct.
pub fn load_kernel(data: &[u8], base: u64) {
    let table = SymbolTable::from_elf(data, base);
    let count = table.symtab_entries;
    KERNEL_SYMS.store(Box::into_raw(Box::new(table)), Ordering::Release);
    log!("symbols: loaded {} kernel symbols", count);
}

/// Resolve and log an address against kernel symbols. Lock-free, allocation-free.
/// Safe to call from any context including panic, double fault, NMI.
pub fn resolve_kernel(addr: u64) -> Option<u64> {
    let ptr = KERNEL_SYMS.load(Ordering::Acquire);
    if ptr.is_null() {
        log!("    {:#x}", addr);
        return None;
    }
    let table = unsafe { &*ptr };
    if let Some((raw, offset)) = table.resolve(addr) {
        log!("    {:#x}  {:#}+{:#x}", addr, rustc_demangle::demangle(raw), offset);
        Some(offset)
    } else {
        let kb = KERNEL_BASE.load(Ordering::Relaxed);
        if kb != 0 && addr >= kb {
            log!("    {:#x}  [kernel+{:#x}]", addr, addr - kb);
        } else {
            log!("    {:#x}", addr);
        }
        None
    }
}

/// Resolve and log a user address against a process's symbol table.
/// Returns true if the address could be identified.
pub fn resolve_user(syms: &SymbolTable, addr: u64) -> bool {
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
