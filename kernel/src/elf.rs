use alloc::alloc::{alloc_zeroed, Layout};

use crate::arch::paging::PAGE_2M;
use crate::log;
use elf::ElfBytes;
use elf::endian::AnyEndian;
use elf::abi::{PT_LOAD, PT_TLS, ET_DYN, EM_X86_64, R_X86_64_RELATIVE};

pub struct LoadedElf {
    pub entry: u64,
    pub base: u64,
    pub base_ptr: *mut u8,
    pub load_size: usize,
    /// Runtime address of TLS template data (.tdata) in the loaded image.
    pub tls_template: u64,
    /// Size of initialized TLS data (.tdata).
    pub tls_filesz: usize,
    /// Total TLS size (.tdata + .tbss).
    pub tls_memsz: usize,
}

/// Parse, validate, and load an ELF binary into memory.
///
/// Allocates page-aligned memory, copies PT_LOAD segments, and applies relocations.
/// Returns the entry point and allocation info, or an error message.
pub fn load(data: &[u8]) -> Result<LoadedElf, &'static str> {
    let elf = match ElfBytes::<AnyEndian>::minimal_parse(data) {
        Ok(e) => e,
        Err(_) => return Err("ELF: parse error"),
    };

    let ehdr = &elf.ehdr;
    if ehdr.e_type != ET_DYN {
        return Err("ELF: not PIE (expected ET_DYN)");
    }
    if ehdr.e_machine != EM_X86_64 {
        return Err("ELF: not x86_64");
    }

    let segments = match elf.segments() {
        Some(s) => s,
        None => return Err("ELF: no program headers"),
    };

    log!("ELF: valid header, entry={:#x}, {} phdrs", ehdr.e_entry, ehdr.e_phnum);

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
        return Err("ELF: no loadable segments");
    }

    let load_size = ((vaddr_max - vaddr_min) as usize + PAGE_2M as usize - 1) & !(PAGE_2M as usize - 1);

    // Allocate 2MB-aligned memory for the loaded image
    let layout = match Layout::from_size_align(load_size, PAGE_2M as usize) {
        Ok(l) => l,
        Err(_) => return Err("ELF: invalid layout"),
    };
    let base_ptr = unsafe { alloc_zeroed(layout) };
    if base_ptr.is_null() {
        return Err("ELF: allocation failed");
    }
    let base = base_ptr as u64 - vaddr_min;
    log!("ELF: allocated {} bytes at {:#x}, base={:#x}", load_size, base_ptr as u64, base);

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

    log!("ELF: {} relocations applied", reloc_count);

    // Parse PT_TLS segment for thread-local storage
    let mut tls_template = 0u64;
    let mut tls_filesz = 0usize;
    let mut tls_memsz = 0usize;
    for phdr in segments.iter() {
        if phdr.p_type == PT_TLS {
            tls_template = base + phdr.p_vaddr;
            tls_filesz = phdr.p_filesz as usize;
            tls_memsz = phdr.p_memsz as usize;
            log!("ELF: TLS template at {:#x}, filesz={}, memsz={}", tls_template, tls_filesz, tls_memsz);
        }
    }

    Ok(LoadedElf {
        entry: base + ehdr.e_entry,
        base,
        base_ptr,
        load_size,
        tls_template,
        tls_filesz,
        tls_memsz,
    })
}
