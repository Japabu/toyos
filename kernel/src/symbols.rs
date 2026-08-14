use core::fmt::{self, Display, Write};
use core::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

use alloc::boxed::Box;
use elf::ElfBytes;
use elf::endian::AnyEndian;
use toyos_abi::log::MAX_RECORD_MESSAGE;

use crate::process::PageAlloc;

/// What is left of a record's message once a backtrace frame's own text has
/// taken its share.
///
/// A frame is `    {addr:#x}  {symbol}+{offset:#x}`: four spaces, an address of
/// at most eighteen characters, two spaces, a `+` and an offset of at most
/// eighteen more. 64 is that with room to spare, and spare is the right
/// direction to be wrong in — a symbol a byte over this loses a byte from its
/// middle, where a *line* a byte over the record's bound loses its tail.
const FRAME_OVERHEAD: usize = 64;
const SYMBOL_BUDGET: usize = MAX_RECORD_MESSAGE - FRAME_OVERHEAD;

/// How much of the budget the head keeps; [`SYMBOL_TAIL`] is the rest.
///
/// An even split, because a backtrace with only one end of a name in it names
/// nothing either way: the head is the crate and the module path, the tail is
/// the function, and `screen_late_panic` asserts on the tail for exactly that
/// reason.
const SYMBOL_HEAD: usize = SYMBOL_BUDGET / 2;
const SYMBOL_TAIL: usize = SYMBOL_BUDGET - SYMBOL_HEAD;

/// A demangled symbol, rendered head-and-tail when it is wider than a record
/// can carry.
///
/// **No bound on the record fixes an unbounded symbol.** A demangled Rust name
/// is bounded by nothing the kernel controls — `late_panic::Nest` is a generic
/// nested in itself and nothing stops it being nested again — so
/// [`MAX_RECORD_MESSAGE`] can only be raised until the *ordinary* line fits,
/// which is what 992 is. What is left is to decide which bytes of an over-wide
/// name survive, and the answer is both ends. Truncation keeps the head and
/// drops the function, which is the half a backtrace is read for.
///
/// This is a producer's decision and not a reader's: the record then holds a
/// whole message, so `elided` still means what the ABI says it means and every
/// consumer renders one thing. The marker is ASCII because the panel's font is
/// codepoints 0x20..=0x7E and the one reader this line has on the machine it
/// matters on is looking at that panel.
///
/// `specs/issues/diagnostics/a-record-cannot-hold-a-demangled-frame.md` was the
/// entry and carries the ruling.
struct Elided<D>(D);

impl<D: Display> Display for Elided<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut count = Count(0);
        // Infallible: `Count` never fails, so this measures rather than renders.
        let _ = write!(count, "{:#}", self.0);
        if count.0 <= SYMBOL_BUDGET {
            return write!(f, "{:#}", self.0);
        }
        let mut both = HeadTail { out: f, seen: 0, shown: 0, tail: [0; SYMBOL_TAIL], filled: 0 };
        write!(both, "{:#}", self.0)?;
        both.finish()
    }
}

/// A `fmt::Write` that measures and writes nowhere.
struct Count(usize);

impl Write for Count {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.0 = self.0.saturating_add(s.len());
        Ok(())
    }
}

/// Passes a name's head straight through and keeps its tail in a fixed buffer,
/// so what an over-wide name loses is its middle.
struct HeadTail<'a, 'b> {
    out: &'a mut fmt::Formatter<'b>,
    /// Bytes the name has produced.
    seen: usize,
    /// Bytes of head already written out. Below [`SYMBOL_HEAD`] by up to three
    /// when the bound falls inside a character.
    shown: usize,
    tail: [u8; SYMBOL_TAIL],
    filled: usize,
}

impl HeadTail<'_, '_> {
    /// Keep the last [`SYMBOL_TAIL`] bytes, contiguously.
    ///
    /// A ring would save the shifting and cost the seam: a character split
    /// across the wrap has no `&str` to be part of. The shifts are a few
    /// hundred bytes per chunk on a path that is already formatting.
    fn keep_tail(&mut self, bytes: &[u8]) {
        if let Some(last) = bytes.len().checked_sub(SYMBOL_TAIL) {
            self.tail.copy_from_slice(&bytes[last..]);
            self.filled = SYMBOL_TAIL;
            return;
        }
        let drop = (self.filled + bytes.len()).saturating_sub(SYMBOL_TAIL);
        if drop > 0 {
            self.tail.copy_within(drop..self.filled, 0);
            self.filled -= drop;
        }
        let end = self.filled + bytes.len();
        if let Some(slot) = self.tail.get_mut(self.filled..end) {
            slot.copy_from_slice(bytes);
            self.filled = end;
        }
    }

    fn finish(self) -> fmt::Result {
        // The tail's first byte is wherever the shifting left it, which can be
        // inside a character; at most three bytes of one can be.
        let mut tail: &[u8] = self.tail.get(..self.filled).unwrap_or(&[]);
        for _ in 0..3 {
            if core::str::from_utf8(tail).is_ok() {
                break;
            }
            tail = tail.get(1..).unwrap_or(&[]);
        }
        let text = core::str::from_utf8(tail).unwrap_or("");
        let elided = self.seen.saturating_sub(self.shown).saturating_sub(text.len());
        write!(self.out, "...[{elided} bytes elided]...{text}")
    }
}

impl Write for HeadTail<'_, '_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let bytes = s.as_bytes();
        if self.shown == self.seen && self.seen < SYMBOL_HEAD {
            let room = SYMBOL_HEAD - self.seen;
            if bytes.len() <= room {
                self.out.write_str(s)?;
                self.shown += bytes.len();
            } else {
                let mut fit = room;
                while fit > 0 && !s.is_char_boundary(fit) {
                    fit -= 1;
                }
                self.out.write_str(s.get(..fit).unwrap_or(""))?;
                self.shown += fit;
            }
        }
        self.seen += bytes.len();
        self.keep_tail(bytes);
        Ok(())
    }
}

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
        log!("    {:#x}  {}+{:#x}", addr, Elided(rustc_demangle::demangle(raw)), offset);
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
        log!("    {:#x}  {}+{:#x}", addr, Elided(rustc_demangle::demangle(name)), offset);
        true
    } else if syms.is_valid_user_addr(addr) {
        let base_offset = addr.saturating_sub(syms.prog_base());
        log!("    {:#x}  [exe+{:#x}]", addr, base_offset);
        true
    } else {
        false
    }
}
