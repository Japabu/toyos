use alloc::borrow::Cow;

use crate::arch::paging::{self, PAGE_2M};
use crate::log;
use crate::process::OwnedAlloc;
use elf::ElfBytes;
use elf::endian::AnyEndian;
use elf::abi::{
    PT_LOAD, PT_TLS, PT_DYNAMIC, ET_DYN, EM_X86_64, R_X86_64_RELATIVE, R_X86_64_GLOB_DAT,
    DT_SYMTAB, DT_STRTAB, DT_STRSZ, DT_NULL, DT_NEEDED, SHT_DYNSYM,
};

const R_X86_64_TPOFF64: u32 = 18;
const R_X86_64_TPOFF32: u32 = 23;

/// Read a null-terminated string from `base + offset`, bounded by `max_size`.
/// Returns `""` if the offset is out of bounds or no null terminator is found.
fn bounded_cstr(base: u64, offset: u64, max_size: u64) -> &'static str {
    if offset >= max_size { return ""; }
    let ptr = (base + offset) as *const u8;
    let remaining = (max_size - offset) as usize;
    let mut len = 0;
    while len < remaining {
        if unsafe { *ptr.add(len) } == 0 { break; }
        len += 1;
    }
    let bytes = unsafe { core::slice::from_raw_parts(ptr, len) };
    core::str::from_utf8(bytes).unwrap_or("")
}

/// Metadata about a loaded ELF binary (no ownership — the OwnedAlloc is separate).
pub struct LoadedElfInfo {
    pub entry: u64,
    pub base: u64,
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
/// Returns the owning allocation and metadata, or an error message.
pub fn load(data: &[u8]) -> Result<(OwnedAlloc, LoadedElfInfo), &'static str> {
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
    let mut vaddr_range: Option<(u64, u64)> = None;
    for phdr in segments.iter().filter(|p| p.p_type == PT_LOAD) {
        let lo = phdr.p_vaddr;
        let hi = phdr.p_vaddr + phdr.p_memsz;
        vaddr_range = Some(match vaddr_range {
            None => (lo, hi),
            Some((min, max)) => (min.min(lo), max.max(hi)),
        });
    }
    let (vaddr_min, vaddr_max) = vaddr_range.ok_or("ELF: no loadable segments")?;

    let load_size = paging::align_2m((vaddr_max - vaddr_min) as usize);

    // Allocate 2MB-aligned memory for the loaded image
    let alloc = match OwnedAlloc::new(load_size, PAGE_2M as usize) {
        Some(a) => a,
        None => return Err("ELF: allocation failed"),
    };
    let base_ptr = alloc.ptr();
    let base = base_ptr as u64 - vaddr_min;
    log!("ELF: allocated {} bytes at {:#x}, base={:#x}", load_size, base_ptr as u64, base);

    // Load PT_LOAD segments (BSS is already zero from alloc_zeroed)
    for phdr in segments.iter() {
        if phdr.p_type == PT_LOAD {
            let dst = (base + phdr.p_vaddr) as *mut u8;
            let src = &data[phdr.p_offset as usize..][..phdr.p_filesz as usize];
            unsafe {
                core::ptr::copy_nonoverlapping(src.as_ptr(), dst, phdr.p_filesz as usize);
            }
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

    Ok((alloc, LoadedElfInfo {
        entry: base + ehdr.e_entry,
        base,
        tls_template,
        tls_filesz,
        tls_memsz,
    }))
}

// ── Dynamic linking ──────────────────────────────────────────────────────

pub struct LoadedLib {
    pub alloc: OwnedAlloc,
    pub base: u64,
    pub dynsym: u64,
    pub dynstr: u64,
    pub dynstr_size: u64,
    pub sym_count: usize,
    pub tls_template: u64,
    pub tls_filesz: usize,
    pub tls_memsz: usize,
    /// Runtime address of .rela.dyn (RELA entries for GLOB_DAT etc.)
    pub rela_addr: u64,
    /// Size of .rela.dyn in bytes.
    pub rela_size: u64,
    /// Runtime address of .rela.plt (JUMP_SLOT entries).
    pub jmprel_addr: u64,
    /// Size of .rela.plt in bytes.
    pub jmprel_size: u64,
    /// Runtime address of .gnu.hash table (0 if absent).
    pub gnu_hash: u64,
}

/// Compute the GNU hash of a symbol name (DJB hash variant).
fn gnu_hash(name: &str) -> u32 {
    let mut h: u32 = 5381;
    for &b in name.as_bytes() {
        h = h.wrapping_mul(33).wrapping_add(b as u32);
    }
    h
}

/// Fast symbol lookup using .gnu.hash table.
fn gnu_dlsym(lib: &LoadedLib, name: &str) -> Option<u64> {
    if lib.gnu_hash == 0 {
        return dlsym(lib, name);
    }
    let hash_addr = lib.gnu_hash;
    let h = gnu_hash(name);

    // Parse header: nbuckets, symoffset, bloom_size, bloom_shift
    let nbuckets = unsafe { *((hash_addr) as *const u32) };
    let symoffset = unsafe { *((hash_addr + 4) as *const u32) };
    let bloom_size = unsafe { *((hash_addr + 8) as *const u32) };
    let bloom_shift = unsafe { *((hash_addr + 12) as *const u32) };

    // Bloom filter check (64-bit words)
    let bloom_base = hash_addr + 16;
    let bloom_word = unsafe {
        *((bloom_base + ((h as u64 / 64) % bloom_size as u64) * 8) as *const u64)
    };
    let mask = (1u64 << (h % 64)) | (1u64 << ((h >> bloom_shift) % 64));
    if bloom_word & mask != mask {
        return None;
    }

    // Bucket lookup
    let buckets_base = bloom_base + bloom_size as u64 * 8;
    let bucket_idx = h % nbuckets;
    let sym_idx = unsafe { *((buckets_base + bucket_idx as u64 * 4) as *const u32) };
    if sym_idx == 0 {
        return None;
    }

    // Chain walk
    let chains_base = buckets_base + nbuckets as u64 * 4;
    let mut i = sym_idx;
    loop {
        let chain_val = unsafe {
            *((chains_base + (i - symoffset) as u64 * 4) as *const u32)
        };
        // Compare hash values (ignoring lowest bit which is the chain-end flag)
        if (chain_val | 1) == (h | 1) {
            // Hash matches — verify with string compare
            let sym_ptr = (lib.dynsym + i as u64 * 24) as *const u8;
            let st_name = unsafe { *(sym_ptr as *const u32) };
            let st_shndx = unsafe { *(sym_ptr.add(6) as *const u16) };
            let st_value = unsafe { *(sym_ptr.add(8) as *const u64) };
            if st_shndx != 0 {
                let sym_name = bounded_cstr(lib.dynstr, st_name as u64, lib.dynstr_size);
                if sym_name == name {
                    return Some(lib.base + st_value);
                }
            }
        }
        if chain_val & 1 != 0 {
            break; // End of chain
        }
        i += 1;
    }
    None
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
    let mut vaddr_range: Option<(u64, u64)> = None;
    for phdr in segments.iter().filter(|p| p.p_type == PT_LOAD) {
        let lo = phdr.p_vaddr;
        let hi = phdr.p_vaddr + phdr.p_memsz;
        vaddr_range = Some(match vaddr_range {
            None => (lo, hi),
            Some((min, max)) => (min.min(lo), max.max(hi)),
        });
    }
    let (vaddr_min, vaddr_max) = vaddr_range.ok_or("dlopen: no loadable segments")?;

    let load_size = paging::align_2m((vaddr_max - vaddr_min) as usize);
    let alloc = match OwnedAlloc::new(load_size, PAGE_2M as usize) {
        Some(a) => a,
        None => return Err("dlopen: allocation failed"),
    };
    let base_ptr = alloc.ptr();
    let base = base_ptr as u64 - vaddr_min;

    // Copy PT_LOAD segments (BSS is already zero from alloc_zeroed)
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

    // Parse PT_DYNAMIC to find DT_SYMTAB, DT_STRTAB, DT_STRSZ, DT_RELA, DT_RELASZ, DT_JMPREL, DT_PLTRELSZ
    let mut symtab_vaddr = 0u64;
    let mut strtab_vaddr = 0u64;
    let mut strtab_size = 0u64;
    let mut rela_vaddr = 0u64;
    let mut rela_size = 0u64;
    let mut jmprel_vaddr = 0u64;
    let mut jmprel_size = 0u64;
    const DT_RELA: i64 = 7;
    const DT_RELASZ: i64 = 8;
    const DT_JMPREL: i64 = 23;
    const DT_PLTRELSZ: i64 = 2;
    const DT_GNU_HASH: i64 = 0x6ffffef5u64 as i64;
    let mut gnu_hash_vaddr = 0u64;
    for phdr in segments.iter() {
        if phdr.p_type == PT_DYNAMIC {
            let dyn_addr = (base + phdr.p_vaddr) as *const u8;
            let dyn_size = phdr.p_filesz as usize;
            let mut offset = 0;
            while offset + 16 <= dyn_size {
                let d_tag = unsafe { *(dyn_addr.add(offset) as *const i64) };
                let d_val = unsafe { *(dyn_addr.add(offset + 8) as *const u64) };
                match d_tag {
                    DT_SYMTAB => symtab_vaddr = d_val,
                    DT_STRTAB => strtab_vaddr = d_val,
                    DT_STRSZ => strtab_size = d_val,
                    DT_RELA => rela_vaddr = d_val,
                    DT_RELASZ => rela_size = d_val,
                    DT_JMPREL => jmprel_vaddr = d_val,
                    DT_PLTRELSZ => jmprel_size = d_val,
                    DT_GNU_HASH => gnu_hash_vaddr = d_val,
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

    // Parse PT_TLS for thread-local storage
    let mut tls_template = 0u64;
    let mut tls_filesz = 0usize;
    let mut tls_memsz = 0usize;
    for phdr in segments.iter() {
        if phdr.p_type == PT_TLS {
            tls_template = base + phdr.p_vaddr;
            tls_filesz = phdr.p_filesz as usize;
            tls_memsz = phdr.p_memsz as usize;
        }
    }

    log!("dlopen: loaded {} bytes at {:#x}, {} relocs, {} dynsyms",
        load_size, base_ptr as u64, reloc_count, sym_count);

    Ok(LoadedLib { alloc, base, dynsym, dynstr, dynstr_size: strtab_size, sym_count,
        tls_template, tls_filesz, tls_memsz,
        rela_addr: base + rela_vaddr, rela_size,
        jmprel_addr: base + jmprel_vaddr, jmprel_size,
        gnu_hash: if gnu_hash_vaddr != 0 { base + gnu_hash_vaddr } else { 0 } })
}

/// Look up a symbol by name in a loaded shared library.
pub fn dlsym(lib: &LoadedLib, name: &str) -> Option<u64> {
    // Each Elf64_Sym is 24 bytes: st_name(4), st_info(1), st_other(1), st_shndx(2), st_value(8), st_size(8)
    for i in 1..lib.sym_count {
        let sym_ptr = (lib.dynsym + i as u64 * 24) as *const u8;
        let st_name = unsafe { *(sym_ptr as *const u32) };
        let st_shndx = unsafe { *(sym_ptr.add(6) as *const u16) };
        let st_value = unsafe { *(sym_ptr.add(8) as *const u64) };

        if st_shndx == 0 {
            continue;
        }

        let sym_name = bounded_cstr(lib.dynstr, st_name as u64, lib.dynstr_size);
        if sym_name == name {
            return Some(lib.base + st_value);
        }
    }
    None
}

/// Resolve GLOB_DAT and JUMP_SLOT relocations in a dlopen'd library by looking
/// up symbols from already-loaded libraries.
pub fn resolve_dlopen_relocs(lib: &LoadedLib, other_libs: &[LoadedLib]) {
    let entry_size = 24u64; // sizeof(Elf64_Rela)
    let mut resolved_count = 0u64;
    let mut unresolved_count = 0u64;

    // Process both .rela.dyn and .rela.plt sections
    let sections = [
        (lib.rela_addr, lib.rela_size),
        (lib.jmprel_addr, lib.jmprel_size),
    ];

    for (rela_addr, rela_size) in sections {
        if rela_size == 0 { continue; }
        let count = rela_size / entry_size;
        for i in 0..count {
            let rela_ptr = (rela_addr + i * entry_size) as *const u8;
            let r_offset = unsafe { *(rela_ptr as *const u64) };
            let r_info = unsafe { *(rela_ptr.add(8) as *const u64) };
            let r_type = (r_info & 0xFFFF_FFFF) as u32;
            let r_sym = (r_info >> 32) as u32;

            match r_type {
                7 /* R_X86_64_JUMP_SLOT */ | 6 /* R_X86_64_GLOB_DAT */ => {
                    let sym_entry = (lib.dynsym + r_sym as u64 * 24) as *const u8;
                    let st_name = unsafe { *(sym_entry as *const u32) };
                    let sym_name = bounded_cstr(lib.dynstr, st_name as u64, lib.dynstr_size);

                    let mut resolved = None;
                    for other in other_libs {
                        if let Some(addr) = gnu_dlsym(other, sym_name) {
                            resolved = Some(addr);
                            break;
                        }
                    }

                    if let Some(resolved) = resolved {
                        let target = (lib.base + r_offset) as *mut u64;
                        unsafe { *target = resolved; }
                        resolved_count += 1;
                    } else {
                        if unresolved_count < 5 {
                            log!("dlopen: unresolved: {}", sym_name);
                        }
                        unresolved_count += 1;
                    }
                }
                _ => {}
            }
        }
    }
    log!("dlopen: resolved {} relocs, {} unresolved", resolved_count, unresolved_count);
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
    let mut strtab_size = 0u64;
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
                DT_STRSZ => strtab_size = d_val,
                DT_NULL => break,
                _ => {}
            }
            offset += 16;
        }
    }

    if needed_offsets.is_empty() {
        return Ok(alloc::vec::Vec::new());
    }

    let dynstr = base + strtab_vaddr;
    let exe_dir = exe_path.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");

    let mut libs = alloc::vec::Vec::new();

    for &name_offset in &needed_offsets {
        let lib_name = bounded_cstr(dynstr, name_offset, strtab_size);

        let lib_path = alloc::format!("{}/{}", exe_dir, lib_name);
        log!("dynamic: loading {} for {}", lib_path, exe_path);

        let so_data = match read_file(&lib_path) {
            Ok(d) => d,
            Err(_) => {
                // Fall back to /lib/ for shared libraries
                let fallback = alloc::format!("/lib/{}", lib_name);
                log!("dynamic: fallback to {}", fallback);
                match read_file(&fallback) {
                    Ok(d) => d,
                    Err(e) => return Err(alloc::format!("{}: {}", lib_name, e)),
                }
            }
        };

        match load_shared_lib(&so_data) {
            Ok(lib) => {
                log!("dynamic: loaded {} at {:#x} ({} syms)", lib_name, lib.alloc.ptr() as u64, lib.sym_count);
                libs.push(lib);
            }
            Err(e) => return Err(alloc::format!("failed to load {}: {}", lib_name, e)),
        }
    }

    // Apply GLOB_DAT relocations: resolve imported symbols from loaded libs.
    // Read RELA entries from the raw file data (not loaded image) because
    // .rela.dyn may not be covered by any PT_LOAD segment.
    let symtab_ptr = (base + symtab_vaddr) as *const u8;
    for section_name in &[".rela.dyn", ".rela.plt"] {
        if let Ok(Some(shdr)) = elf.section_header_by_name(section_name) {
            if let Ok(relas) = elf.section_data_as_relas(&shdr) {
                for rela in relas {
                    match rela.r_type {
                        R_X86_64_RELATIVE => {
                            let target = (base + rela.r_offset) as *mut u64;
                            unsafe { *target = (base as i64 + rela.r_addend) as u64; }
                        }
                        R_X86_64_GLOB_DAT => {
                            let sym_idx = rela.r_sym as u64;
                            let sym_entry = unsafe { symtab_ptr.add(sym_idx as usize * 24) };
                            let st_name = unsafe { *(sym_entry as *const u32) };
                            let sym_name = bounded_cstr(dynstr, st_name as u64, strtab_size);

                            let resolved = libs.iter()
                                .find_map(|lib| gnu_dlsym(lib, sym_name));

                            match resolved {
                                Some(addr) => {
                                    let target = (base + rela.r_offset) as *mut u64;
                                    unsafe { *target = addr; }
                                }
                                None => log!("dynamic: unresolved symbol: {}", sym_name),
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    // Resolve GLOB_DAT in shared libraries — look up symbols from the
    // executable and from other loaded libraries. This enables shared libraries
    // to call back into the executable (e.g., _start calling main).
    let exe_dlsym = |name: &str| -> Option<u64> {
        if let Ok(Some((symtab, strtab))) = elf.symbol_table() {
            for sym in symtab.iter() {
                if sym.st_shndx == 0 { continue; }
                if let Ok(sym_name) = strtab.get(sym.st_name as usize) {
                    if sym_name == name {
                        return Some(base + sym.st_value);
                    }
                }
            }
        }
        None
    };

    for lib in &libs {
        let sections = [
            (lib.rela_addr, lib.rela_size),
            (lib.jmprel_addr, lib.jmprel_size),
        ];
        let entry_size = 24u64; // sizeof(Elf64_Rela)
        for (rela_addr, rela_size) in sections {
        if rela_size == 0 { continue; }
        let count = rela_size / entry_size;
        for i in 0..count {
            let rela_ptr = (rela_addr + i * entry_size) as *const u8;
            let r_offset = unsafe { *(rela_ptr as *const u64) };
            let r_info = unsafe { *(rela_ptr.add(8) as *const u64) };
            let r_addend = unsafe { *(rela_ptr.add(16) as *const i64) };
            let r_type = (r_info & 0xFFFF_FFFF) as u32;
            let r_sym = (r_info >> 32) as u32;

            match r_type {
                7 /* R_X86_64_JUMP_SLOT */ | 6 /* R_X86_64_GLOB_DAT */ => {
                    let sym_entry = (lib.dynsym + r_sym as u64 * 24) as *const u8;
                    let st_name = unsafe { *(sym_entry as *const u32) };
                    let sym_name = bounded_cstr(lib.dynstr, st_name as u64, lib.dynstr_size);

                    // Search: executable first, then other libraries
                    let resolved = exe_dlsym(sym_name)
                        .or_else(|| libs.iter().find_map(|other| gnu_dlsym(other, sym_name)));

                    if let Some(addr) = resolved {
                        let target = (lib.base + r_offset) as *mut u64;
                        unsafe { *target = addr; }
                        if sym_name == "main" {
                            log!("dynamic: resolved main -> {:#x}", addr);
                        }
                    } else {
                        log!("dynamic: lib unresolved symbol: {}", sym_name);
                    }
                }
                8 /* R_X86_64_RELATIVE */ => {
                    let target = (lib.base + r_offset) as *mut u64;
                    unsafe { *target = (lib.base as i64 + r_addend) as u64; }
                }
                _ => {}
            }
        }
        } // for sections
    }

    Ok(libs)
}

/// TLS layout info for cross-library TPOFF resolution.
pub struct TlsModuleInfo<'a> {
    pub libs: &'a [LoadedLib],
    /// (tls_template, tls_filesz, tls_memsz, base_offset) for each TLS module
    pub modules: &'a [(u64, usize, usize, usize)],
}

/// Apply R_X86_64_TPOFF64 and R_X86_64_TPOFF32 relocations in a shared library.
/// `lib_base_offset` is this library's TLS placement within the combined TLS block.
/// `total_memsz` is the total combined TLS size across all modules.
/// TPOFF64: fills a GOT slot (u64) with base_offset + addend - total_memsz
/// TPOFF32: patches an inline immediate (i32) with base_offset + addend - total_memsz
/// When r_sym != 0, the symbol is looked up across all loaded libraries to find the
/// defining library's TLS base offset.
pub fn apply_tpoff_relocs(lib: &LoadedLib, lib_base_offset: usize, total_memsz: usize, tls_info: &TlsModuleInfo) {
    let entry_size = 24u64;
    let sections = [
        (lib.rela_addr, lib.rela_size),
        (lib.jmprel_addr, lib.jmprel_size),
    ];
    let mut count64 = 0u64;
    let mut count32 = 0u64;
    for (rela_addr, rela_size) in sections {
        if rela_size == 0 { continue; }
        let num = rela_size / entry_size;
        for i in 0..num {
            let rela_ptr = (rela_addr + i * entry_size) as *const u8;
            let r_offset = unsafe { *(rela_ptr as *const u64) };
            let r_info = unsafe { *(rela_ptr.add(8) as *const u64) };
            let r_addend = unsafe { *(rela_ptr.add(16) as *const i64) };
            let r_type = (r_info & 0xFFFF_FFFF) as u32;
            let r_sym = (r_info >> 32) as u32;

            if r_type == R_X86_64_TPOFF64 {
                let tpoff = if r_sym != 0 {
                    // Check if symbol is locally defined (st_shndx != 0)
                    let sym_entry = (lib.dynsym + r_sym as u64 * 24) as *const u8;
                    let st_shndx = unsafe { *(sym_entry.add(6) as *const u16) };
                    if st_shndx != 0 {
                        // Locally defined TLS symbol — use this library's offset
                        let st_value = unsafe { *(sym_entry.add(8) as *const u64) };
                        lib_base_offset as i64 + st_value as i64 + r_addend - total_memsz as i64
                    } else {
                        // Undefined — resolve from other loaded libraries
                        resolve_cross_lib_tpoff(lib, r_sym, tls_info, total_memsz)
                    }
                } else {
                    lib_base_offset as i64 + r_addend - total_memsz as i64
                };
                let target = (lib.base + r_offset) as *mut u64;
                unsafe { *target = tpoff as u64; }
                count64 += 1;
            } else if r_type == R_X86_64_TPOFF32 {
                let tpoff = if r_sym != 0 {
                    let sym_entry = (lib.dynsym + r_sym as u64 * 24) as *const u8;
                    let st_shndx = unsafe { *(sym_entry.add(6) as *const u16) };
                    if st_shndx != 0 {
                        let st_value = unsafe { *(sym_entry.add(8) as *const u64) };
                        lib_base_offset as i64 + st_value as i64 + r_addend - total_memsz as i64
                    } else {
                        resolve_cross_lib_tpoff(lib, r_sym, tls_info, total_memsz)
                    }
                } else {
                    lib_base_offset as i64 + r_addend - total_memsz as i64
                };
                let target = (lib.base + r_offset) as *mut i32;
                unsafe { *target = tpoff as i32; }
                count32 += 1;
            }
        }
    }
    if count64 > 0 || count32 > 0 {
        log!("dlopen: applied {} TPOFF64 + {} TPOFF32 relocs (base_offset={}, total_memsz={})",
            count64, count32, lib_base_offset, total_memsz);
    }
}

/// Look up a TLS symbol by name in a library's .dynsym, returning its st_value
/// (offset within the TLS segment). Returns None if not found.
fn tls_dlsym(lib: &LoadedLib, name: &str) -> Option<u64> {
    // Linear scan of dynsym since gnu_hash adds base which is wrong for TLS
    for idx in 0..lib.sym_count {
        let sym_ptr = (lib.dynsym + idx as u64 * 24) as *const u8;
        let st_name = unsafe { *(sym_ptr as *const u32) };
        let st_info = unsafe { *sym_ptr.add(4) };
        let st_shndx = unsafe { *(sym_ptr.add(6) as *const u16) };
        let st_value = unsafe { *(sym_ptr.add(8) as *const u64) };
        // STT_TLS = 6
        if st_shndx != 0 && (st_info & 0xf) == 6 {
            let sym_name = bounded_cstr(lib.dynstr, st_name as u64, lib.dynstr_size);
            if sym_name == name {
                return Some(st_value);
            }
        }
    }
    None
}

/// Resolve a cross-library TPOFF64 relocation by looking up the symbol name
/// from `lib`'s dynsym, finding which library defines it, and computing the tpoff.
fn resolve_cross_lib_tpoff(lib: &LoadedLib, r_sym: u32, tls_info: &TlsModuleInfo, total_memsz: usize) -> i64 {
    // Read symbol name from lib's dynsym/dynstr
    let sym_entry = (lib.dynsym + r_sym as u64 * 24) as *const u8;
    let st_name = unsafe { *(sym_entry as *const u32) };
    let sym_name = bounded_cstr(lib.dynstr, st_name as u64, lib.dynstr_size);

    // Search all libraries for the defining TLS symbol
    for other_lib in tls_info.libs {
        if other_lib.tls_memsz == 0 { continue; }
        if let Some(sym_tls_offset) = tls_dlsym(other_lib, sym_name) {
            // Find this library's base offset in the combined layout
            let other_base_offset = tls_info.modules.iter()
                .find(|&&(template, _, _, _)| template == other_lib.tls_template)
                .map(|&(_, _, _, bo)| bo)
                .unwrap_or(0);
            return other_base_offset as i64 + sym_tls_offset as i64 - total_memsz as i64;
        }
    }
    log!("tpoff: unresolved TLS symbol: {}", sym_name);
    0
}

/// Apply R_X86_64_TPOFF64/TPOFF32 relocations in the main executable's .rela.dyn.
/// Uses section headers from the raw ELF data to find relocation entries.
/// When r_sym != 0, looks up the TLS symbol across loaded libraries.
pub fn apply_exe_tpoff_relocs(data: &[u8], base: u64, exe_base_offset: usize, total_memsz: usize, tls_info: &TlsModuleInfo) {
    let elf = match ElfBytes::<AnyEndian>::minimal_parse(data) {
        Ok(e) => e,
        Err(_) => return,
    };

    // Get dynsym/dynstr for resolving named symbols (r_sym != 0)
    let (dynsym_data, dynstr_data) = match (
        elf.section_header_by_name(".dynsym"),
        elf.section_header_by_name(".dynstr"),
    ) {
        (Ok(Some(sym_shdr)), Ok(Some(str_shdr))) => {
            let sym_data = elf.section_data(&sym_shdr).ok().map(|(d, _)| d);
            let str_data = elf.section_data(&str_shdr).ok().map(|(d, _)| d);
            (sym_data, str_data)
        }
        _ => (None, None),
    };

    let mut count = 0u64;
    for section_name in &[".rela.dyn", ".rela.plt"] {
        if let Ok(Some(shdr)) = elf.section_header_by_name(section_name) {
            if let Ok(relas) = elf.section_data_as_relas(&shdr) {
                for rela in relas {
                    if rela.r_type == R_X86_64_TPOFF64 as u32 {
                        let tpoff = if rela.r_sym != 0 {
                            // Check if locally defined before searching other libs
                            let locally_defined = dynsym_data.map(|d| {
                                let off = rela.r_sym as usize * 24;
                                off + 24 <= d.len() && u16::from_le_bytes(d[off + 6..off + 8].try_into().unwrap()) != 0
                            }).unwrap_or(false);
                            if locally_defined {
                                let d = dynsym_data.unwrap();
                                let off = rela.r_sym as usize * 24;
                                let st_value = u64::from_le_bytes(d[off + 8..off + 16].try_into().unwrap());
                                exe_base_offset as i64 + st_value as i64 + rela.r_addend - total_memsz as i64
                            } else {
                                resolve_exe_cross_lib_tpoff(rela.r_sym, &dynsym_data, &dynstr_data, tls_info, total_memsz)
                            }
                        } else {
                            exe_base_offset as i64 + rela.r_addend - total_memsz as i64
                        };
                        let target = (base + rela.r_offset) as *mut u64;
                        unsafe { *target = tpoff as u64; }
                        count += 1;
                    } else if rela.r_type == R_X86_64_TPOFF32 as u32 {
                        let tpoff = if rela.r_sym != 0 {
                            let locally_defined = dynsym_data.map(|d| {
                                let off = rela.r_sym as usize * 24;
                                off + 24 <= d.len() && u16::from_le_bytes(d[off + 6..off + 8].try_into().unwrap()) != 0
                            }).unwrap_or(false);
                            if locally_defined {
                                let d = dynsym_data.unwrap();
                                let off = rela.r_sym as usize * 24;
                                let st_value = u64::from_le_bytes(d[off + 8..off + 16].try_into().unwrap());
                                exe_base_offset as i64 + st_value as i64 + rela.r_addend - total_memsz as i64
                            } else {
                                resolve_exe_cross_lib_tpoff(rela.r_sym, &dynsym_data, &dynstr_data, tls_info, total_memsz)
                            }
                        } else {
                            exe_base_offset as i64 + rela.r_addend - total_memsz as i64
                        };
                        let target = (base + rela.r_offset) as *mut i32;
                        unsafe { *target = tpoff as i32; }
                        count += 1;
                    }
                }
            }
        }
    }
    if count > 0 {
        log!("exe: applied {} TPOFF relocs (base_offset={}, total_memsz={})",
            count, exe_base_offset, total_memsz);
    }
}

/// Resolve a cross-library TPOFF64 from the main executable by looking up the
/// symbol name from the exe's dynsym/dynstr tables, then finding which library defines it.
fn resolve_exe_cross_lib_tpoff(r_sym: u32, dynsym_data: &Option<&[u8]>, dynstr_data: &Option<&[u8]>, tls_info: &TlsModuleInfo, total_memsz: usize) -> i64 {
    let (Some(dynsym), Some(dynstr)) = (dynsym_data, dynstr_data) else {
        log!("tpoff: no dynsym/dynstr for exe cross-lib TLS");
        return 0;
    };
    let entry_offset = r_sym as usize * 24;
    if entry_offset + 24 > dynsym.len() {
        log!("tpoff: r_sym {} out of bounds", r_sym);
        return 0;
    }
    let st_name = u32::from_le_bytes(dynsym[entry_offset..entry_offset + 4].try_into().unwrap()) as usize;
    if st_name >= dynstr.len() {
        log!("tpoff: st_name {} out of bounds", st_name);
        return 0;
    }
    let name_end = dynstr[st_name..].iter().position(|&b| b == 0).unwrap_or(dynstr.len() - st_name);
    let sym_name = core::str::from_utf8(&dynstr[st_name..st_name + name_end]).unwrap_or("");

    for lib in tls_info.libs {
        if lib.tls_memsz == 0 { continue; }
        if let Some(sym_tls_offset) = tls_dlsym(lib, sym_name) {
            let other_base_offset = tls_info.modules.iter()
                .find(|&&(template, _, _, _)| template == lib.tls_template)
                .map(|&(_, _, _, bo)| bo)
                .unwrap_or(0);
            return other_base_offset as i64 + sym_tls_offset as i64 - total_memsz as i64;
        }
    }
    log!("tpoff: unresolved exe TLS symbol: {}", sym_name);
    0
}
