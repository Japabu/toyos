use alloc::alloc::{alloc_zeroed, Layout};
use core::arch::asm;

use alloc::format;
use crate::{gdt, log, serial, syscall};

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

unsafe fn read_struct<T>(data: &[u8], offset: usize) -> &T {
    assert!(offset + core::mem::size_of::<T>() <= data.len());
    &*(data.as_ptr().add(offset) as *const T)
}

pub fn run(data: &[u8]) {
    // Validate ELF header
    if data.len() < core::mem::size_of::<Elf64Ehdr>() {
        log::println("ELF: file too small");
        return;
    }

    let ehdr = unsafe { read_struct::<Elf64Ehdr>(data, 0) };

    if ehdr.e_ident[0..4] != ELF_MAGIC {
        log::println("ELF: bad magic");
        return;
    }
    if ehdr.e_ident[4] != ELFCLASS64 {
        log::println("ELF: not 64-bit");
        return;
    }
    if ehdr.e_machine != EM_X86_64 {
        log::println("ELF: not x86_64");
        return;
    }
    if ehdr.e_type != ET_DYN {
        log::println("ELF: not PIE (expected ET_DYN)");
        return;
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
        return;
    }

    let load_size = (vaddr_max - vaddr_min) as usize;
    let load_align = 4096usize;

    // Allocate memory for the loaded image
    let layout = match Layout::from_size_align(load_size, load_align) {
        Ok(l) => l,
        Err(_) => {
            log::println("ELF: invalid layout");
            return;
        }
    };
    let base_ptr = unsafe { alloc_zeroed(layout) };
    if base_ptr.is_null() {
        log::println("ELF: allocation failed");
        return;
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

    // Allocate stack for the program
    let stack_layout = Layout::from_size_align(USER_STACK_SIZE, 16).unwrap();
    let stack_base = unsafe { alloc_zeroed(stack_layout) };
    if stack_base.is_null() {
        log::println("ELF: stack allocation failed");
        return;
    }
    let stack_top = stack_base as u64 + USER_STACK_SIZE as u64;

    // Execute the program
    serial::println(&format!("ELF: entry={:#x}, stack={:#x}", entry, stack_top));
    let exit_code = execute(entry, stack_top);
    log::println(if exit_code == 0 { "Process exited" } else { "Process exited with error" });

    // TODO: free program memory and stack
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
