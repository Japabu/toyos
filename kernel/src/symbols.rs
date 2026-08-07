use core::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

use alloc::boxed::Box;
use elf::ElfBytes;
use elf::endian::AnyEndian;

use crate::process::PageAlloc;

/// Zero-allocation symbol table. Points directly into ELF sections in memory.
/// Resolution is a linear scan over raw Elf64_Sym entries — O(n) but lock-free,
/// allocation-free, and safe to call from any context including panic/double-fault.
///
/// **That property is the reason for the raw pointers**, and it is what decides
/// where the bytes may come from: the resolve path is reached from the fault
/// handler and from the panic handler, so it may not allocate, may not take a
/// lock and may not do I/O. Everything expensive happens once, when the table
/// is built.
pub struct SymbolTable {
    /// What keeps the bytes below mapped, when this table owns them.
    ///
    /// `None` for the kernel's own tables: they point into the ELF the
    /// bootloader left in the direct map, which outlives every reader. A
    /// process's tables are read off its file into these pages, so they have to
    /// die with it — and the pointers survive the struct moving, because 2 MiB
    /// physical pages do not move when a `Vec` header does.
    pages: Option<PageAlloc>,
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

// Safety: the bytes are either the kernel image, mapped for the machine's
// lifetime, or pages this table owns and frees.
unsafe impl Send for SymbolTable {}
unsafe impl Sync for SymbolTable {}

impl SymbolTable {
    pub fn empty() -> Self {
        Self::empty_with_bounds(0, 0, 0, 0)
    }

    pub fn empty_with_bounds(
        prog_base: u64, prog_end: u64,
        stack_base: u64, stack_end: u64,
    ) -> Self {
        Self {
            pages: None,
            symtab: core::ptr::null(),
            symtab_entries: 0,
            strtab: core::ptr::null(),
            strtab_len: 0,
            base: 0,
            prog_base, prog_end, stack_base, stack_end,
        }
    }

    /// A process's tables, in pages it owns: `symtab_len` bytes of `.symtab` at
    /// the start, `strtab_len` bytes of `.strtab` immediately after.
    ///
    /// Laid out by one caller, [`crate::loader::symbols::read_backtrace_table`],
    /// which is also what read them — so the two halves cannot be given
    /// separately and cannot disagree about where the second one starts.
    pub fn from_pages(
        pages: PageAlloc,
        symtab_len: usize, entry_size: usize,
        strtab_len: usize,
        base: u64,
        prog_base: u64, prog_end: u64,
        stack_base: u64, stack_end: u64,
    ) -> Self {
        let start = pages.ptr();
        Self {
            symtab: start,
            symtab_entries: symtab_len / entry_size,
            strtab: unsafe { start.add(symtab_len) },
            strtab_len,
            pages: Some(pages),
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
            pages: None,
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

    /// How much memory this table's bytes occupy, for the spawn log.
    pub fn resident_bytes(&self) -> usize {
        self.pages.as_ref().map_or(0, PageAlloc::size)
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

    /// [`resolve`](Self::resolve) for a *return address*.
    ///
    /// A return address is the instruction after the `call`, so when the call
    /// is the last instruction of its function — every call to a diverging
    /// function, and any tail position — it lands one byte past the symbol's
    /// last byte and `resolve` correctly refuses it. Every backtrace frame but
    /// the innermost is a return address, which is why panic reports read
    /// `[kernel+0x…]` far more often than the symbol table's coverage explains.
    pub fn resolve_return(&self, return_addr: u64) -> Option<(&str, u64)> {
        let (name, offset) = self.resolve(return_addr.saturating_sub(1))?;
        Some((name, offset + 1))
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
    log_kernel(addr, |table| table.resolve(addr))
}

/// [`resolve_kernel`] for a backtrace frame's return address — see
/// [`SymbolTable::resolve_return`].
pub fn resolve_kernel_return(return_addr: u64) -> Option<u64> {
    log_kernel(return_addr, |table| table.resolve_return(return_addr))
}

fn log_kernel(addr: u64, lookup: impl FnOnce(&SymbolTable) -> Option<(&str, u64)>) -> Option<u64> {
    let ptr = KERNEL_SYMS.load(Ordering::Acquire);
    if ptr.is_null() {
        log!("    {:#x}", addr);
        return None;
    }
    let table = unsafe { &*ptr };
    if let Some((raw, offset)) = lookup(table) {
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
    log_user(syms, addr, syms.resolve(addr))
}

/// [`resolve_user`] for a backtrace frame's return address — see
/// [`SymbolTable::resolve_return`].
pub fn resolve_user_return(syms: &SymbolTable, return_addr: u64) -> bool {
    log_user(syms, return_addr, syms.resolve_return(return_addr))
}

fn log_user(syms: &SymbolTable, addr: u64, resolved: Option<(&str, u64)>) -> bool {
    if let Some((name, offset)) = resolved {
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
