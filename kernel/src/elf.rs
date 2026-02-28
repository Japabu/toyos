use alloc::alloc::{alloc_zeroed, Layout};
use alloc::borrow::Cow;

use crate::arch::paging::PAGE_2M;
use crate::log;
use elf::ElfBytes;
use elf::endian::AnyEndian;
use elf::abi::{
    PT_LOAD, PT_TLS, PT_DYNAMIC, ET_DYN, EM_X86_64, R_X86_64_RELATIVE, R_X86_64_GLOB_DAT,
    DT_SYMTAB, DT_STRTAB, DT_NULL, DT_NEEDED, DT_RELA, DT_RELASZ, SHT_DYNSYM,
};

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

// ── Dynamic linking ──────────────────────────────────────────────────────

pub struct LoadedLib {
    pub base: u64,
    pub base_ptr: *mut u8,
    pub load_size: usize,
    pub dynsym: u64,
    pub dynstr: u64,
    pub sym_count: usize,
}

/// Load a shared library (.so) into memory for dynamic linking.
/// Like `load()` but parses PT_DYNAMIC for .dynsym/.dynstr symbol tables.
pub fn load_shared_lib(data: &[u8]) -> Result<LoadedLib, &'static str> {
    let elf = match ElfBytes::<AnyEndian>::minimal_parse(data) {
        Ok(e) => e,
        Err(_) => return Err("dlopen: ELF parse error"),
    };

    let ehdr = &elf.ehdr;
    if ehdr.e_type != ET_DYN {
        return Err("dlopen: not a shared library (expected ET_DYN)");
    }
    if ehdr.e_machine != EM_X86_64 {
        return Err("dlopen: not x86_64");
    }

    let segments = match elf.segments() {
        Some(s) => s,
        None => return Err("dlopen: no program headers"),
    };

    // Scan PT_LOAD for address range
    let mut vaddr_min: u64 = u64::MAX;
    let mut vaddr_max: u64 = 0;
    for phdr in segments.iter() {
        if phdr.p_type == PT_LOAD {
            vaddr_min = vaddr_min.min(phdr.p_vaddr);
            vaddr_max = vaddr_max.max(phdr.p_vaddr + phdr.p_memsz);
        }
    }
    if vaddr_min == u64::MAX {
        return Err("dlopen: no loadable segments");
    }

    let load_size = ((vaddr_max - vaddr_min) as usize + PAGE_2M as usize - 1) & !(PAGE_2M as usize - 1);
    let layout = match Layout::from_size_align(load_size, PAGE_2M as usize) {
        Ok(l) => l,
        Err(_) => return Err("dlopen: invalid layout"),
    };
    let base_ptr = unsafe { alloc_zeroed(layout) };
    if base_ptr.is_null() {
        return Err("dlopen: allocation failed");
    }
    let base = base_ptr as u64 - vaddr_min;

    // Copy PT_LOAD segments
    for phdr in segments.iter() {
        if phdr.p_type == PT_LOAD {
            let dst = (base + phdr.p_vaddr) as *mut u8;
            let src = &data[phdr.p_offset as usize..][..phdr.p_filesz as usize];
            unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), dst, phdr.p_filesz as usize); }
        }
    }

    // Apply R_X86_64_RELATIVE relocations
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

    // Parse PT_DYNAMIC to find DT_SYMTAB and DT_STRTAB (file offsets in the loaded image)
    let mut symtab_vaddr = 0u64;
    let mut strtab_vaddr = 0u64;
    for phdr in segments.iter() {
        if phdr.p_type == PT_DYNAMIC {
            // Read dynamic entries from loaded memory
            let dyn_addr = (base + phdr.p_vaddr) as *const u8;
            let dyn_size = phdr.p_filesz as usize;
            let mut offset = 0;
            while offset + 16 <= dyn_size {
                let d_tag = unsafe { *(dyn_addr.add(offset) as *const i64) };
                let d_val = unsafe { *(dyn_addr.add(offset + 8) as *const u64) };
                match d_tag {
                    DT_SYMTAB => symtab_vaddr = d_val,
                    DT_STRTAB => strtab_vaddr = d_val,
                    DT_NULL => break,
                    _ => {}
                }
                offset += 16;
            }
        }
    }

    // Count .dynsym entries from section header
    let mut sym_count = 0;
    if let Some(shdrs) = elf.section_headers() {
        for shdr in shdrs.iter() {
            if shdr.sh_type == SHT_DYNSYM {
                sym_count = (shdr.sh_size / shdr.sh_entsize.max(24)) as usize;
                break;
            }
        }
    }

    let dynsym = base + symtab_vaddr;
    let dynstr = base + strtab_vaddr;

    log!("dlopen: loaded {} bytes at {:#x}, {} relocs, {} dynsyms",
        load_size, base_ptr as u64, reloc_count, sym_count);

    Ok(LoadedLib { base, base_ptr, load_size, dynsym, dynstr, sym_count })
}

/// Look up a symbol by name in a loaded shared library.
/// Returns the runtime address, or 0 if not found.
pub fn dlsym(lib: &LoadedLib, name: &str) -> u64 {
    // Each Elf64_Sym is 24 bytes: st_name(4), st_info(1), st_other(1), st_shndx(2), st_value(8), st_size(8)
    for i in 1..lib.sym_count {
        let sym_ptr = (lib.dynsym + i as u64 * 24) as *const u8;
        let st_name = unsafe { *(sym_ptr as *const u32) };
        let st_shndx = unsafe { *(sym_ptr.add(6) as *const u16) };
        let st_value = unsafe { *(sym_ptr.add(8) as *const u64) };

        // Skip undefined symbols (st_shndx == 0)
        if st_shndx == 0 {
            continue;
        }

        // Read symbol name from dynstr
        let name_ptr = (lib.dynstr + st_name as u64) as *const u8;
        let sym_name = unsafe {
            let mut len = 0;
            while *name_ptr.add(len) != 0 { len += 1; }
            core::str::from_utf8_unchecked(core::slice::from_raw_parts(name_ptr, len))
        };

        if sym_name == name {
            return lib.base + st_value;
        }
    }
    0
}

/// Load DT_NEEDED shared libraries and apply GLOB_DAT relocations for a
/// dynamically-linked executable. Called after `load()` during process spawn.
///
/// Reads the executable's PT_DYNAMIC to find DT_NEEDED entries, loads each .so
/// from the same directory as the executable, and resolves GLOB_DAT relocations
/// (writing resolved symbol addresses into GOT slots).
pub fn resolve_dynamic_deps(
    data: &[u8],
    base: u64,
    exe_path: &str,
    read_file: impl Fn(&str) -> Result<Cow<'static, [u8]>, &'static str>,
) -> Result<alloc::vec::Vec<LoadedLib>, alloc::string::String> {
    let elf = match ElfBytes::<AnyEndian>::minimal_parse(data) {
        Ok(e) => e,
        Err(_) => return Ok(alloc::vec::Vec::new()),
    };

    let segments = match elf.segments() {
        Some(s) => s,
        None => return Ok(alloc::vec::Vec::new()),
    };

    // Find PT_DYNAMIC and parse dynamic entries
    let mut symtab_vaddr = 0u64;
    let mut strtab_vaddr = 0u64;
    let mut rela_vaddr = 0u64;
    let mut rela_size = 0u64;
    let mut needed_offsets = alloc::vec::Vec::new();

    for phdr in segments.iter() {
        if phdr.p_type != PT_DYNAMIC {
            continue;
        }
        let dyn_addr = (base + phdr.p_vaddr) as *const u8;
        let dyn_size = phdr.p_filesz as usize;
        let mut offset = 0;
        while offset + 16 <= dyn_size {
            let d_tag = unsafe { *(dyn_addr.add(offset) as *const i64) };
            let d_val = unsafe { *(dyn_addr.add(offset + 8) as *const u64) };
            match d_tag {
                DT_NEEDED => needed_offsets.push(d_val),
                DT_SYMTAB => symtab_vaddr = d_val,
                DT_STRTAB => strtab_vaddr = d_val,
                DT_RELA => rela_vaddr = d_val,
                DT_RELASZ => rela_size = d_val,
                DT_NULL => break,
                _ => {}
            }
            offset += 16;
        }
    }

    if needed_offsets.is_empty() {
        return Ok(alloc::vec::Vec::new());
    }

    // Read DT_NEEDED filenames from .dynstr (loaded in memory at base + strtab_vaddr)
    let strtab_ptr = (base + strtab_vaddr) as *const u8;
    let exe_dir = exe_path.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");

    let mut libs = alloc::vec::Vec::new();

    for &name_offset in &needed_offsets {
        let name_ptr = unsafe { strtab_ptr.add(name_offset as usize) };
        let lib_name = unsafe {
            let mut len = 0;
            while *name_ptr.add(len) != 0 { len += 1; }
            core::str::from_utf8_unchecked(core::slice::from_raw_parts(name_ptr, len))
        };

        let lib_path = alloc::format!("{}/{}", exe_dir, lib_name);
        log!("dynamic: loading {} for {}", lib_path, exe_path);

        let so_data = match read_file(&lib_path) {
            Ok(d) => d,
            Err(e) => return Err(alloc::format!("{}: {}", lib_path, e)),
        };

        match load_shared_lib(&so_data) {
            Ok(lib) => {
                log!("dynamic: loaded {} at {:#x} ({} syms)", lib_name, lib.base_ptr as u64, lib.sym_count);
                libs.push(lib);
            }
            Err(e) => return Err(alloc::format!("failed to load {}: {}", lib_name, e)),
        }
    }

    // Apply GLOB_DAT relocations: resolve imported symbols from loaded libs
    if rela_vaddr != 0 && rela_size > 0 {
        let rela_ptr = (base + rela_vaddr) as *const u8;
        let num_entries = rela_size / 24;
        let symtab_ptr = (base + symtab_vaddr) as *const u8;

        for i in 0..num_entries {
            let entry = unsafe { rela_ptr.add(i as usize * 24) };
            let r_offset = unsafe { *(entry as *const u64) };
            let r_info = unsafe { *(entry.add(8) as *const u64) };
            let r_type = (r_info & 0xFFFFFFFF) as u32;

            match r_type {
                R_X86_64_RELATIVE => {
                    // Already handled by load(), but handle here too for completeness
                    let r_addend = unsafe { *(entry.add(16) as *const i64) };
                    let target = (base + r_offset) as *mut u64;
                    unsafe { *target = (base as i64 + r_addend) as u64; }
                }
                R_X86_64_GLOB_DAT => {
                    let sym_idx = (r_info >> 32) as u64;
                    // Read symbol name from executable's import .dynsym/.dynstr
                    let sym_entry = unsafe { symtab_ptr.add(sym_idx as usize * 24) };
                    let st_name = unsafe { *(sym_entry as *const u32) };
                    let name_ptr = unsafe { strtab_ptr.add(st_name as usize) };
                    let sym_name = unsafe {
                        let mut len = 0;
                        while *name_ptr.add(len) != 0 { len += 1; }
                        core::str::from_utf8_unchecked(core::slice::from_raw_parts(name_ptr, len))
                    };

                    // Search all loaded libs for this symbol
                    let mut resolved = 0u64;
                    for lib in &libs {
                        let addr = dlsym(lib, sym_name);
                        if addr != 0 {
                            resolved = addr;
                            break;
                        }
                    }

                    if resolved == 0 {
                        log!("dynamic: unresolved symbol: {}", sym_name);
                    }

                    let target = (base + r_offset) as *mut u64;
                    unsafe { *target = resolved; }
                }
                _ => {}
            }
        }
    }

    Ok(libs)
}
