use alloc::alloc::{alloc_zeroed, Layout};
use alloc::string::String;
use alloc::vec::Vec;
use core::arch::asm;

use alloc::format;
use elf::ElfBytes;
use elf::endian::AnyEndian;
use elf::abi::{PT_LOAD, ET_DYN, EM_X86_64, R_X86_64_RELATIVE, STT_FUNC};
use crate::arch::{gdt, paging, syscall};
use crate::drivers::serial;
use crate::log;

const USER_STACK_SIZE: usize = 64 * 1024; // 64 KB

// --- Symbol table for crash diagnostics ---

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

pub fn clear_symbols() {
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
pub fn resolve_symbol(addr: u64) -> Option<(String, u64)> {
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
fn load_symbols(data: &[u8], base: u64) {
    let table = unsafe { &mut *(&raw mut PROCESS_SYMS) };
    parse_symtab(data, base, table);
    serial::println(&format!("ELF: loaded {} function symbols", table.symbols.len()));
}

/// Load kernel symbols from raw ELF bytes. Called once at boot.
pub fn load_kernel_symbols(data: &[u8], base: u64) {
    let table = unsafe { &mut *(&raw mut KERNEL_SYMS) };
    parse_symtab(data, base, table);
    serial::println(&format!("Kernel: loaded {} function symbols", table.symbols.len()));
}

pub fn run(data: &[u8], args: &[&str]) -> i32 {
    // Parse ELF
    let elf = match ElfBytes::<AnyEndian>::minimal_parse(data) {
        Ok(e) => e,
        Err(e) => {
            log::println(&format!("ELF: parse error: {}", e));
            return -1;
        }
    };

    let ehdr = &elf.ehdr;
    if ehdr.e_type != ET_DYN {
        log::println("ELF: not PIE (expected ET_DYN)");
        return -1;
    }
    if ehdr.e_machine != EM_X86_64 {
        log::println("ELF: not x86_64");
        return -1;
    }

    let segments = match elf.segments() {
        Some(s) => s,
        None => {
            log::println("ELF: no program headers");
            return -1;
        }
    };

    serial::println(&format!("ELF: valid header, entry={:#x}, {} phdrs", ehdr.e_entry, ehdr.e_phnum));

    // Scan PT_LOAD segments to find total virtual address range
    let mut vaddr_min: u64 = u64::MAX;
    let mut vaddr_max: u64 = 0;

    for phdr in segments.iter() {
        if phdr.p_type == PT_LOAD {
            vaddr_min = vaddr_min.min(phdr.p_vaddr);
            vaddr_max = vaddr_max.max(phdr.p_vaddr + phdr.p_memsz);
        }
    }

    if vaddr_min == u64::MAX {
        log::println("ELF: no loadable segments");
        return -1;
    }

    let load_size = ((vaddr_max - vaddr_min) as usize + 4095) & !4095; // page-align up

    // Allocate page-aligned memory for the loaded image
    let layout = match Layout::from_size_align(load_size, 4096) {
        Ok(l) => l,
        Err(_) => {
            log::println("ELF: invalid layout");
            return -1;
        }
    };
    let base_ptr = unsafe { alloc_zeroed(layout) };
    if base_ptr.is_null() {
        log::println("ELF: allocation failed");
        return -1;
    }
    let base = base_ptr as u64 - vaddr_min;
    serial::println(&format!("ELF: allocated {} bytes at {:#x}, base={:#x}", load_size, base_ptr as u64, base));

    // Load PT_LOAD segments
    for phdr in segments.iter() {
        if phdr.p_type == PT_LOAD {
            let dst = (base + phdr.p_vaddr) as *mut u8;
            let src = &data[phdr.p_offset as usize..][..phdr.p_filesz as usize];
            unsafe {
                core::ptr::copy_nonoverlapping(src.as_ptr(), dst, phdr.p_filesz as usize);
            }
            // BSS (memsz > filesz) is already zero from alloc_zeroed
        }
    }

    // Apply relocations from .rela.dyn and .rela.plt sections
    let mut reloc_count = 0u64;
    for section_name in &[".rela.dyn", ".rela.plt"] {
        if let Ok(Some(shdr)) = elf.section_header_by_name(section_name) {
            if let Ok(relas) = elf.section_data_as_relas(&shdr) {
                for rela in relas {
                    if rela.r_type == R_X86_64_RELATIVE {
                        let target = (base + rela.r_offset) as *mut u64;
                        let value = (base as i64 + rela.r_addend) as u64;
                        unsafe { *target = value; }
                        reloc_count += 1;
                    }
                }
            }
        }
    }

    serial::println(&format!("ELF: {} relocations applied", reloc_count));
    let entry = base + ehdr.e_entry;

    // Mark ELF memory as user-accessible
    paging::map_user(base_ptr as u64, load_size as u64);

    // Allocate page-aligned stack for the program
    let stack_layout = Layout::from_size_align(USER_STACK_SIZE, 4096).unwrap();
    let stack_base = unsafe { alloc_zeroed(stack_layout) };
    if stack_base.is_null() {
        log::println("ELF: stack allocation failed");
        return -1;
    }
    let stack_top = stack_base as u64 + USER_STACK_SIZE as u64;
    paging::map_user(stack_base as u64, USER_STACK_SIZE as u64);

    // Load symbols for crash diagnostics (before data goes out of scope)
    load_symbols(data, base);
    // Track memory ranges for backtrace address validation
    unsafe {
        (&raw mut PROG_BASE).write(base_ptr as u64);
        (&raw mut PROG_END).write(base_ptr as u64 + load_size as u64);
        (&raw mut STACK_BASE).write(stack_base as u64);
        (&raw mut STACK_END).write(stack_top);
    }

    // Set up user heap for syscall allocations
    crate::user_heap::init();

    // Write argc/argv onto the user stack (Linux-style layout).
    // Stack grows down from stack_top. We place:
    //   1. Null-terminated arg strings at the top
    //   2. argc (u64) + argv[] pointer array + NULL below, 16-byte aligned
    // RSP will point to argc on entry.
    let mut sp = stack_top;

    // 1. Write arg strings at top of stack, collect their user-space addresses
    let mut argv_ptrs: Vec<u64> = Vec::with_capacity(args.len());
    for arg in args.iter().rev() {
        sp -= (arg.len() + 1) as u64; // +1 for null terminator
        unsafe {
            core::ptr::copy_nonoverlapping(arg.as_ptr(), sp as *mut u8, arg.len());
            *((sp + arg.len() as u64) as *mut u8) = 0; // null terminator
        }
        argv_ptrs.push(sp);
    }
    argv_ptrs.reverse(); // restore original order

    // 2. Reserve space for metadata (argc + argv[0..n] + NULL), align to 16
    let metadata_qwords = args.len() + 2; // argc + argv pointers + NULL
    sp = (sp - metadata_qwords as u64 * 8) & !15;

    // 3. Write argc at sp, argv pointers at sp+8.., NULL terminator at end
    unsafe {
        *(sp as *mut u64) = args.len() as u64; // argc
        for (i, ptr) in argv_ptrs.iter().enumerate() {
            *((sp + 8 + i as u64 * 8) as *mut u64) = *ptr;
        }
        *((sp + 8 + args.len() as u64 * 8) as *mut u64) = 0; // NULL
    }

    serial::println(&format!("ELF: entry={:#x}, stack={:#x}, argc={}", entry, sp, args.len()));
    let exit_code = execute(entry, sp);

    // Clear symbols on process exit
    clear_symbols();

    // TODO: free program memory and stack
    exit_code
}

fn execute(entry: u64, stack_top: u64) -> i32 {
    let code: u64;
    unsafe {
        let krsp_ptr = &raw mut syscall::SYSCALL_KERNEL_RSP as *mut u64;
        let tss_rsp0_ptr = gdt::tss_rsp0_ptr();
        (&raw mut syscall::PROCESS_ACTIVE).write(true);

        // Save callee-saved registers on the kernel stack, then enter ring 3
        // via iretq. sys_exit restores the kernel RSP and `ret`s to label 2:.
        asm!(
            // Save callee-saved registers on kernel stack
            "push rbp",
            "push rbx",
            "push r12",
            "push r13",
            "push r14",
            "push r15",

            // Push return address (sys_exit will `ret` here)
            "lea rax, [rip + 2f]",
            "push rax",

            // Save kernel RSP for syscall entry, sys_exit, and ring-3 interrupts
            "mov [{krsp}], rsp",
            "mov [{tss_rsp0}], rsp",

            // Enter ring 3 via iretq
            "push 0x1B",        // SS:  user_data | RPL=3
            "push {stack}",     // RSP: user stack
            "push 0x202",       // RFLAGS: IF=1
            "push 0x23",        // CS:  user_code | RPL=3
            "push {entry}",     // RIP: entry point
            "iretq",

            // sys_exit restores kernel RSP and `ret`s here
            "2:",
            "sti",              // re-enable interrupts (FMASK cleared IF on last syscall)

            // Restore callee-saved registers
            "pop r15",
            "pop r14",
            "pop r13",
            "pop r12",
            "pop rbx",
            "pop rbp",

            krsp = in(reg) krsp_ptr,
            tss_rsp0 = in(reg) tss_rsp0_ptr,
            entry = in(reg) entry,
            stack = in(reg) stack_top,
            out("rax") code,
            clobber_abi("sysv64"),
        );
    }
    code as i32
}
