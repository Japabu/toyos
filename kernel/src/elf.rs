use alloc::alloc::{alloc_zeroed, Layout};
use alloc::string::String;
use alloc::vec::Vec;
use core::arch::asm;

use alloc::format;
use crate::{gdt, log, paging, serial, syscall};

// ELF constants
const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
const ELFCLASS64: u8 = 2;
const EM_X86_64: u16 = 62;
const ET_DYN: u16 = 3; // PIE executables are ET_DYN
const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const DT_RELA: i64 = 7;
const DT_RELASZ: i64 = 8;
const DT_RELAENT: i64 = 9;
const R_X86_64_RELATIVE: u32 = 8;

// Section header types
const SHT_SYMTAB: u32 = 2;

// Symbol binding/type from st_info
const STT_FUNC: u8 = 2;

const USER_STACK_SIZE: usize = 64 * 1024; // 64 KB

// ELF64 header
#[repr(C)]
#[derive(Debug)]
struct Elf64Ehdr {
    e_ident: [u8; 16],
    e_type: u16,
    e_machine: u16,
    e_version: u32,
    e_entry: u64,
    e_phoff: u64,
    e_shoff: u64,
    e_flags: u32,
    e_ehsize: u16,
    e_phentsize: u16,
    e_phnum: u16,
    e_shentsize: u16,
    e_shnum: u16,
    e_shstrndx: u16,
}

// ELF64 program header
#[repr(C)]
#[derive(Debug)]
struct Elf64Phdr {
    p_type: u32,
    p_flags: u32,
    p_offset: u64,
    p_vaddr: u64,
    p_paddr: u64,
    p_filesz: u64,
    p_memsz: u64,
    p_align: u64,
}

// ELF64 section header
#[repr(C)]
struct Elf64Shdr {
    sh_name: u32,
    sh_type: u32,
    sh_flags: u64,
    sh_addr: u64,
    sh_offset: u64,
    sh_size: u64,
    sh_link: u32,  // for SHT_SYMTAB: index of associated strtab section
    sh_info: u32,
    sh_addralign: u64,
    sh_entsize: u64,
}

// ELF64 symbol table entry
#[repr(C)]
struct Elf64Sym {
    st_name: u32,
    st_info: u8,
    st_other: u8,
    st_shndx: u16,
    st_value: u64,
    st_size: u64,
}

impl Elf64Sym {
    fn st_type(&self) -> u8 { self.st_info & 0xf }
}

// ELF64 dynamic entry
#[repr(C)]
struct Elf64Dyn {
    d_tag: i64,
    d_val: u64,
}

// ELF64 relocation with addend
#[repr(C)]
struct Elf64Rela {
    r_offset: u64,
    r_info: u64,
    r_addend: i64,
}

impl Elf64Rela {
    fn r_type(&self) -> u32 {
        (self.r_info & 0xFFFF_FFFF) as u32
    }
}

// --- Symbol table for crash diagnostics ---

struct Symbol {
    addr: u64,
    size: u64,
    name_start: u32, // offset into SYMBOL_NAMES
}

// Global symbol data for the current process
static mut SYMBOLS: Vec<Symbol> = Vec::new();
static mut SYMBOL_NAMES: Vec<u8> = Vec::new();
// Memory range of loaded program (for backtrace address validation)
static mut PROG_BASE: u64 = 0;
static mut PROG_END: u64 = 0;
static mut STACK_BASE: u64 = 0;
static mut STACK_END: u64 = 0;

pub fn clear_symbols() {
    unsafe {
        (&raw mut SYMBOLS).as_mut().unwrap().clear();
        (&raw mut SYMBOL_NAMES).as_mut().unwrap().clear();
        (&raw mut PROG_BASE).write(0);
        (&raw mut PROG_END).write(0);
        (&raw mut STACK_BASE).write(0);
        (&raw mut STACK_END).write(0);
    }
}

/// Demangle a Rust symbol name. Handles the legacy `_ZN...E` mangling scheme.
fn demangle(mangled: &str) -> String {
    // Legacy Rust mangling: _ZN {len}{component}... E
    if !mangled.starts_with("_ZN") || !mangled.ends_with('E') {
        return String::from(mangled);
    }

    let inner = &mangled[3..mangled.len() - 1]; // strip _ZN and E
    let mut result = String::new();
    let bytes = inner.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        // Parse decimal length
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if start == i {
            // Not a valid component — return original
            return String::from(mangled);
        }
        let len: usize = match inner[start..i].parse() {
            Ok(n) => n,
            Err(_) => return String::from(mangled),
        };
        if i + len > bytes.len() {
            return String::from(mangled);
        }
        let component = &inner[i..i + len];
        i += len;

        // Skip hash suffix (e.g. "h1234abcdef567890")
        if i == bytes.len() && component.len() == 17 && component.starts_with('h')
            && component[1..].chars().all(|c| c.is_ascii_hexdigit())
        {
            break;
        }

        if !result.is_empty() {
            result.push_str("::");
        }
        result.push_str(component);
    }

    if result.is_empty() {
        String::from(mangled)
    } else {
        result
    }
}

/// Look up a symbol by runtime address. Returns (demangled_name, offset_from_symbol_start).
pub fn resolve_symbol(addr: u64) -> Option<(String, u64)> {
    let symbols = unsafe { &*(&raw const SYMBOLS) };
    let names = unsafe { &*(&raw const SYMBOL_NAMES) };
    if symbols.is_empty() {
        return None;
    }

    // Binary search for the last symbol with addr <= target
    let mut lo = 0usize;
    let mut hi = symbols.len();
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if symbols[mid].addr <= addr {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }

    if lo == 0 {
        return None;
    }
    let sym = &symbols[lo - 1];
    let offset = addr - sym.addr;

    // If the symbol has a known size, only match within that range
    if sym.size > 0 && offset >= sym.size {
        return None;
    }

    let name_start = sym.name_start as usize;
    // Find null terminator
    let name_end = names[name_start..].iter().position(|&b| b == 0)
        .map(|i| name_start + i)
        .unwrap_or(names.len());
    let raw = unsafe { core::str::from_utf8_unchecked(&names[name_start..name_end]) };
    Some((demangle(raw), offset))
}

/// Check if an address is in user-accessible memory (program or stack).
pub fn is_valid_user_addr(addr: u64) -> bool {
    let prog_base = unsafe { *(&raw const PROG_BASE) };
    let prog_end = unsafe { *(&raw const PROG_END) };
    let stack_base = unsafe { *(&raw const STACK_BASE) };
    let stack_end = unsafe { *(&raw const STACK_END) };
    (addr >= prog_base && addr < prog_end) || (addr >= stack_base && addr < stack_end)
}

/// Parse .symtab and its associated .strtab from the raw ELF file bytes.
fn load_symbols(data: &[u8], ehdr: &Elf64Ehdr, base: u64) {
    if ehdr.e_shoff == 0 || ehdr.e_shnum == 0 {
        return;
    }

    let shent = ehdr.e_shentsize as usize;
    if shent < core::mem::size_of::<Elf64Shdr>() {
        return;
    }

    // Find SHT_SYMTAB section
    let mut symtab_shdr: Option<&Elf64Shdr> = None;
    for i in 0..ehdr.e_shnum as usize {
        let off = ehdr.e_shoff as usize + i * shent;
        if off + shent > data.len() { break; }
        let shdr = unsafe { read_struct::<Elf64Shdr>(data, off) };
        if shdr.sh_type == SHT_SYMTAB {
            symtab_shdr = Some(shdr);
            break;
        }
    }

    let symtab = match symtab_shdr {
        Some(s) => s,
        None => return,
    };

    // Get the linked strtab section
    let strtab_idx = symtab.sh_link as usize;
    if strtab_idx >= ehdr.e_shnum as usize { return; }
    let strtab_off = ehdr.e_shoff as usize + strtab_idx * shent;
    if strtab_off + shent > data.len() { return; }
    let strtab_shdr = unsafe { read_struct::<Elf64Shdr>(data, strtab_off) };

    let strtab_start = strtab_shdr.sh_offset as usize;
    let strtab_size = strtab_shdr.sh_size as usize;
    if strtab_start + strtab_size > data.len() { return; }

    // Copy string table
    let names = unsafe { &mut *(&raw mut SYMBOL_NAMES) };
    names.clear();
    names.extend_from_slice(&data[strtab_start..strtab_start + strtab_size]);

    // Parse symbol entries
    let sym_start = symtab.sh_offset as usize;
    let sym_size = symtab.sh_size as usize;
    let sym_ent = symtab.sh_entsize as usize;
    if sym_ent < core::mem::size_of::<Elf64Sym>() || sym_ent == 0 { return; }
    if sym_start + sym_size > data.len() { return; }

    let count = sym_size / sym_ent;
    let symbols = unsafe { &mut *(&raw mut SYMBOLS) };
    symbols.clear();

    for i in 0..count {
        let off = sym_start + i * sym_ent;
        let sym = unsafe { read_struct::<Elf64Sym>(data, off) };
        // Only include function symbols with nonzero address
        if sym.st_type() == STT_FUNC && sym.st_value != 0 {
            symbols.push(Symbol {
                addr: base + sym.st_value,
                size: sym.st_size,
                name_start: sym.st_name,
            });
        }
    }

    // Sort by address for binary search
    symbols.sort_unstable_by_key(|s| s.addr);
    serial::println(&format!("ELF: loaded {} function symbols", symbols.len()));
}

unsafe fn read_struct<T>(data: &[u8], offset: usize) -> &T {
    assert!(offset + core::mem::size_of::<T>() <= data.len());
    &*(data.as_ptr().add(offset) as *const T)
}

pub fn run(data: &[u8]) -> i32 {
    // Validate ELF header
    if data.len() < core::mem::size_of::<Elf64Ehdr>() {
        log::println("ELF: file too small");
        return -1;
    }

    let ehdr = unsafe { read_struct::<Elf64Ehdr>(data, 0) };

    if ehdr.e_ident[0..4] != ELF_MAGIC {
        log::println("ELF: bad magic");
        return -1;
    }
    if ehdr.e_ident[4] != ELFCLASS64 {
        log::println("ELF: not 64-bit");
        return -1;
    }
    if ehdr.e_machine != EM_X86_64 {
        log::println("ELF: not x86_64");
        return -1;
    }
    if ehdr.e_type != ET_DYN {
        log::println("ELF: not PIE (expected ET_DYN)");
        return -1;
    }
    serial::println(&format!("ELF: valid header, entry={:#x}, {} phdrs", ehdr.e_entry, ehdr.e_phnum));

    // Scan PT_LOAD segments to find total virtual address range
    let mut vaddr_min: u64 = u64::MAX;
    let mut vaddr_max: u64 = 0;

    for i in 0..ehdr.e_phnum as usize {
        let phdr_off = ehdr.e_phoff as usize + i * ehdr.e_phentsize as usize;
        let phdr = unsafe { read_struct::<Elf64Phdr>(data, phdr_off) };
        if phdr.p_type == PT_LOAD {
            if phdr.p_vaddr < vaddr_min {
                vaddr_min = phdr.p_vaddr;
            }
            let end = phdr.p_vaddr + phdr.p_memsz;
            if end > vaddr_max {
                vaddr_max = end;
            }
        }
    }

    if vaddr_min == u64::MAX {
        log::println("ELF: no loadable segments");
        return -1;
    }

    let load_size = ((vaddr_max - vaddr_min) as usize + 4095) & !4095; // page-align up
    let load_align = 4096usize;

    // Allocate page-aligned memory for the loaded image
    let layout = match Layout::from_size_align(load_size, load_align) {
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
    for i in 0..ehdr.e_phnum as usize {
        let phdr_off = ehdr.e_phoff as usize + i * ehdr.e_phentsize as usize;
        let phdr = unsafe { read_struct::<Elf64Phdr>(data, phdr_off) };
        if phdr.p_type == PT_LOAD {
            let dst = (base + phdr.p_vaddr) as *mut u8;
            let src = &data[phdr.p_offset as usize..][..phdr.p_filesz as usize];
            unsafe {
                core::ptr::copy_nonoverlapping(src.as_ptr(), dst, phdr.p_filesz as usize);
            }
            // BSS (memsz > filesz) is already zero from alloc_zeroed
        }
    }

    // Find and process relocations via PT_DYNAMIC
    let mut rela_addr: u64 = 0;
    let mut rela_size: u64 = 0;
    let mut rela_ent: u64 = 0;

    for i in 0..ehdr.e_phnum as usize {
        let phdr_off = ehdr.e_phoff as usize + i * ehdr.e_phentsize as usize;
        let phdr = unsafe { read_struct::<Elf64Phdr>(data, phdr_off) };
        if phdr.p_type == PT_DYNAMIC {
            let dyn_start = (base + phdr.p_vaddr) as *const Elf64Dyn;
            let mut j = 0;
            loop {
                let dyn_entry = unsafe { &*dyn_start.add(j) };
                match dyn_entry.d_tag {
                    0 => break, // DT_NULL
                    DT_RELA => rela_addr = dyn_entry.d_val,
                    DT_RELASZ => rela_size = dyn_entry.d_val,
                    DT_RELAENT => rela_ent = dyn_entry.d_val,
                    _ => {}
                }
                j += 1;
            }
        }
    }

    // Process R_X86_64_RELATIVE relocations
    if rela_addr != 0 && rela_ent != 0 && rela_size != 0 {
        let count = rela_size / rela_ent;
        for i in 0..count {
            let rela = unsafe {
                &*((base + rela_addr + i * rela_ent) as *const Elf64Rela)
            };
            if rela.r_type() == R_X86_64_RELATIVE {
                let target = (base + rela.r_offset) as *mut u64;
                let value = (base as i64 + rela.r_addend) as u64;
                unsafe { *target = value; }
            }
        }
    }

    serial::println(&format!("ELF: {} relocations applied", if rela_ent > 0 { rela_size / rela_ent } else { 0 }));
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
    load_symbols(data, ehdr, base);
    // Track memory ranges for backtrace address validation
    unsafe {
        (&raw mut PROG_BASE).write(base_ptr as u64);
        (&raw mut PROG_END).write(base_ptr as u64 + load_size as u64);
        (&raw mut STACK_BASE).write(stack_base as u64);
        (&raw mut STACK_END).write(stack_top);
    }

    // Set up user heap for syscall allocations
    syscall::init_user_heap();

    // Execute the program
    // x86_64 SysV ABI: RSP must be 16n+8 at function entry (as if `call` pushed a return address).
    // iretq doesn't push a return address, so subtract 8 to simulate it.
    let stack_top = stack_top - 8;
    serial::println(&format!("ELF: entry={:#x}, stack={:#x}", entry, stack_top));
    let exit_code = execute(entry, stack_top);

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
