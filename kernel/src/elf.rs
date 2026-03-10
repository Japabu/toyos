use alloc::borrow::Cow;
use alloc::string::String;

use crate::arch::paging::{self, PAGE_2M};
use crate::log;
use crate::process::OwnedAlloc;
use crate::sync::Lock;
use elf::ElfBytes;
use elf::endian::AnyEndian;
use elf::abi::{
    PT_LOAD, PT_TLS, PT_DYNAMIC, ET_DYN, EM_X86_64, R_X86_64_RELATIVE, R_X86_64_GLOB_DAT,
    DT_SYMTAB, DT_STRTAB, DT_STRSZ, DT_NULL, DT_NEEDED, SHT_DYNSYM,
};

const R_X86_64_TPOFF64: u32 = 18;
const R_X86_64_TPOFF32: u32 = 23;

// ── Shared library memory ownership ──────────────────────────────────────

/// Ownership model for a loaded shared library's memory.
pub enum LibMemory {
    /// Fresh load: single allocation owns everything.
    Owned(OwnedAlloc),
    /// Cloned from cache: RO pages are shared (owned by cache), RW pages are private.
    Shared {
        /// Private copy of the RW segment pages (and the straddling 2MB page).
        rw_alloc: OwnedAlloc,
        /// Start address of the shared (cached) region.
        shared_addr: u64,
        /// Size of the shared region in bytes.
        shared_size: usize,
        /// Total size of the cached allocation (shared + private template).
        total_cached_size: usize,
        /// Delta to translate cached RW addresses to private physical addresses.
        /// kernel_write_addr = cached_addr + rw_delta
        rw_delta: i64,
    },
}

// ── Shared library cache ─────────────────────────────────────────────────

/// Cached loaded library — immortal image used as template for shared page cloning.
/// RO pages (code/rodata) are mapped directly into user processes.
/// RW pages (data/GOT) are copied privately per process.
struct CachedLib {
    /// The fully loaded + RELATIVE-relocated image (immortal, never freed).
    alloc: OwnedAlloc,
    alloc_size: usize,
    vaddr_min: u64,
    /// 2MB-aligned offset within alloc: [0..shared_end) is shared RO,
    /// [shared_end..alloc_size) is copied privately per process.
    shared_end_offset: usize,
    /// Metadata offsets relative to base.
    dynsym_off: u64,
    dynstr_off: u64,
    dynstr_size: u64,
    sym_count: usize,
    tls_vaddr: u64,
    tls_filesz: usize,
    tls_memsz: usize,
    rela_off: u64,
    rela_size: u64,
    jmprel_off: u64,
    jmprel_size: u64,
    gnu_hash_off: u64,
}

static SO_CACHE: Lock<alloc::vec::Vec<(String, CachedLib)>> = Lock::new(alloc::vec::Vec::new());

/// Store a loaded library in the cache for future reuse.
/// `rw_vaddr` is the lowest virtual address of a writable PT_LOAD segment.
fn cache_loaded_lib(path: &str, lib: &LoadedLib, rw_vaddr: u64) {
    let alloc = match &lib.memory {
        LibMemory::Owned(a) => a,
        _ => return,
    };
    let size = alloc.size();
    let Some(cache_alloc) = OwnedAlloc::new_uninit(size, PAGE_2M as usize) else { return };
    unsafe {
        core::ptr::copy_nonoverlapping(alloc.ptr(), cache_alloc.ptr(), size);
    }
    let vaddr_min = alloc.ptr() as u64 - lib.base;

    // Compute the 2MB-aligned split between shared (RO) and private (RW) regions.
    // Round DOWN so the straddling page goes into the private copy.
    let rw_offset_in_alloc = (lib.base + rw_vaddr) as usize - alloc.ptr() as usize;
    let shared_end_offset = rw_offset_in_alloc & !(PAGE_2M as usize - 1);

    let cached = CachedLib {
        alloc: cache_alloc,
        alloc_size: size,
        vaddr_min,
        shared_end_offset,
        dynsym_off: lib.dynsym - lib.base,
        dynstr_off: lib.dynstr - lib.base,
        dynstr_size: lib.dynstr_size,
        sym_count: lib.sym_count,
        tls_vaddr: if lib.tls_memsz > 0 { lib.tls_template - lib.base } else { 0 },
        tls_filesz: lib.tls_filesz,
        tls_memsz: lib.tls_memsz,
        rela_off: lib.rela_addr - lib.base,
        rela_size: lib.rela_size,
        jmprel_off: lib.jmprel_addr - lib.base,
        jmprel_size: lib.jmprel_size,
        gnu_hash_off: if lib.gnu_hash != 0 { lib.gnu_hash - lib.base } else { 0 },
    };
    SO_CACHE.lock().push((String::from(path), cached));
}

/// Clone a LoadedLib from a cached image — shares RO pages, copies only RW pages.
/// Base address stays the same as the cache so RELATIVE relocations need zero fixup.
fn clone_from_cache(cached: &CachedLib) -> Option<LoadedLib> {
    let t0 = crate::clock::nanos_since_boot();

    let cached_ptr = cached.alloc.ptr() as u64;
    let base = cached_ptr - cached.vaddr_min;
    let shared_size = cached.shared_end_offset;
    let private_size = cached.alloc_size - cached.shared_end_offset;

    // Allocate and copy only the private (RW) portion
    let aligned_private = paging::align_2m(private_size);
    let rw_alloc = OwnedAlloc::new_uninit(aligned_private, PAGE_2M as usize)?;
    let src = unsafe { cached.alloc.ptr().add(cached.shared_end_offset) };
    unsafe { core::ptr::copy_nonoverlapping(src, rw_alloc.ptr(), private_size); }
    // Zero any trailing bytes in the last 2MB page (BSS-like)
    if aligned_private > private_size {
        unsafe {
            core::ptr::write_bytes(rw_alloc.ptr().add(private_size), 0, aligned_private - private_size);
        }
    }

    let t1 = crate::clock::nanos_since_boot();
    let rw_delta = rw_alloc.ptr() as i64 - (cached_ptr as i64 + shared_size as i64);

    log!("dlopen: cache hit (shared), {}MB shared + {}MB private, copy={}ms",
        shared_size / (1024*1024), private_size / (1024*1024),
        (t1 - t0) / 1_000_000);

    Some(LoadedLib {
        memory: LibMemory::Shared {
            rw_alloc,
            shared_addr: cached_ptr,
            shared_size,
            total_cached_size: cached.alloc_size,
            rw_delta,
        },
        base,
        dynsym: base + cached.dynsym_off,
        dynstr: base + cached.dynstr_off,
        dynstr_size: cached.dynstr_size,
        sym_count: cached.sym_count,
        tls_template: if cached.tls_memsz > 0 { base + cached.tls_vaddr } else { 0 },
        tls_filesz: cached.tls_filesz,
        tls_memsz: cached.tls_memsz,
        rela_addr: base + cached.rela_off,
        rela_size: cached.rela_size,
        jmprel_addr: base + cached.jmprel_off,
        jmprel_size: cached.jmprel_size,
        gnu_hash: if cached.gnu_hash_off != 0 { base + cached.gnu_hash_off } else { 0 },
    })
}

/// Derive total symbol count from a GNU hash table.
/// The table is: [nbuckets, symoffset, bloom_size, bloom_shift, bloom[], buckets[], chain[]]
/// Each bucket holds the lowest symbol index; each chain entry's bit 0 marks the end of a chain.
fn gnu_hash_sym_count(addr: u64) -> usize {
    let ptr = addr as *const u32;
    let nbuckets = unsafe { *ptr } as usize;
    let symoffset = unsafe { *ptr.add(1) } as usize;
    let bloom_size = unsafe { *ptr.add(2) } as usize;
    // Skip header (4 u32s) + bloom (bloom_size u64s = bloom_size*2 u32s)
    let buckets_offset = 4 + bloom_size * 2;
    let buckets_ptr = unsafe { ptr.add(buckets_offset) };
    // Find the max bucket value = highest starting symbol index
    let mut max_sym = 0usize;
    for i in 0..nbuckets {
        let val = unsafe { *buckets_ptr.add(i) } as usize;
        if val > max_sym { max_sym = val; }
    }
    if max_sym < symoffset { return symoffset; }
    // Walk the chain from max_sym until we find the end (bit 0 set)
    let chain_ptr = unsafe { buckets_ptr.add(nbuckets) };
    let mut idx = max_sym - symoffset;
    loop {
        let entry = unsafe { *chain_ptr.add(idx) };
        if entry & 1 != 0 { return symoffset + idx + 1; }
        idx += 1;
    }
}

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

    // Apply RELATIVE relocations — try DT_RELA from PT_DYNAMIC first, fall back to section headers
    let mut rela_vaddr = 0u64;
    let mut rela_size = 0u64;
    let mut jmprel_vaddr = 0u64;
    let mut jmprel_size = 0u64;
    for phdr in segments.iter() {
        if phdr.p_type == PT_DYNAMIC {
            let dyn_addr = (base + phdr.p_vaddr) as *const u8;
            let dyn_size = phdr.p_filesz as usize;
            let mut offset = 0;
            while offset + 16 <= dyn_size {
                let d_tag = unsafe { *(dyn_addr.add(offset) as *const i64) };
                let d_val = unsafe { *(dyn_addr.add(offset + 8) as *const u64) };
                match d_tag {
                    7 /* DT_RELA */ => rela_vaddr = d_val,
                    8 /* DT_RELASZ */ => rela_size = d_val,
                    23 /* DT_JMPREL */ => jmprel_vaddr = d_val,
                    2 /* DT_PLTRELSZ */ => jmprel_size = d_val,
                    0 /* DT_NULL */ => break,
                    _ => {}
                }
                offset += 16;
            }
        }
    }
    let mut reloc_count = 0u64;
    if rela_size > 0 || jmprel_size > 0 {
        // Fast path: use DT_RELA/DT_JMPREL from the loaded image
        let entry_size = 24u64;
        for &(rela_addr, rela_sz) in &[(base + rela_vaddr, rela_size), (base + jmprel_vaddr, jmprel_size)] {
            if rela_sz == 0 { continue; }
            let num = rela_sz / entry_size;
            for i in 0..num {
                let rela_ptr = (rela_addr + i * entry_size) as *const u8;
                let r_info = unsafe { *(rela_ptr.add(8) as *const u64) };
                let r_type = (r_info & 0xFFFF_FFFF) as u32;
                if r_type == R_X86_64_RELATIVE {
                    let r_offset = unsafe { *(rela_ptr as *const u64) };
                    let r_addend = unsafe { *(rela_ptr.add(16) as *const i64) };
                    let target = (base + r_offset) as *mut u64;
                    unsafe { *target = (base as i64 + r_addend) as u64; }
                    reloc_count += 1;
                }
            }
        }
    } else {
        // Fallback: no PT_DYNAMIC or no DT_RELA — parse section headers
        for section_name in &[".rela.dyn", ".rela.plt"] {
            if let Ok(Some(shdr)) = elf.section_header_by_name(section_name) {
                if let Ok(relas) = elf.section_data_as_relas(&shdr) {
                    for rela in relas {
                        if rela.r_type == R_X86_64_RELATIVE {
                            let target = (base + rela.r_offset) as *mut u64;
                            unsafe { *target = (base as i64 + rela.r_addend) as u64; }
                            reloc_count += 1;
                        }
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
    pub memory: LibMemory,
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

impl LoadedLib {
    /// Translate a cached virtual address to the kernel-writable physical address.
    /// For Owned memory, this is identity. For Shared memory, RW addresses are
    /// translated to the private physical pages via rw_delta.
    pub fn rw_write_ptr<T>(&self, cached_addr: u64) -> *mut T {
        match &self.memory {
            LibMemory::Owned(_) => cached_addr as *mut T,
            LibMemory::Shared { rw_delta, .. } => {
                (cached_addr as i64 + rw_delta) as *mut T
            }
        }
    }
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
/// Returns (LoadedLib, rw_vaddr) where rw_vaddr is the lowest writable segment vaddr.
pub fn load_shared_lib(data: &[u8]) -> Result<(LoadedLib, u64), &'static str> {
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

    // Scan PT_LOAD for address range and first writable segment
    let mut vaddr_range: Option<(u64, u64)> = None;
    let mut rw_vaddr: Option<u64> = None;
    for phdr in segments.iter().filter(|p| p.p_type == PT_LOAD) {
        let lo = phdr.p_vaddr;
        let hi = phdr.p_vaddr + phdr.p_memsz;
        vaddr_range = Some(match vaddr_range {
            None => (lo, hi),
            Some((min, max)) => (min.min(lo), max.max(hi)),
        });
        if phdr.p_flags & 0x2 != 0 { // PF_W
            rw_vaddr = Some(rw_vaddr.map_or(lo, |v: u64| v.min(lo)));
        }
    }
    let (vaddr_min, vaddr_max) = vaddr_range.ok_or("dlopen: no loadable segments")?;
    // If no RW segment, treat entire library as read-only (rw_vaddr = end)
    let rw_vaddr = rw_vaddr.unwrap_or(vaddr_max);

    let load_size = paging::align_2m((vaddr_max - vaddr_min) as usize);
    let t0 = crate::clock::nanos_since_boot();
    let alloc = match OwnedAlloc::new_uninit(load_size, PAGE_2M as usize) {
        Some(a) => a,
        None => return Err("dlopen: allocation failed"),
    };
    let t1 = crate::clock::nanos_since_boot();
    let base_ptr = alloc.ptr();
    let base = base_ptr as u64 - vaddr_min;

    // Copy PT_LOAD segments and zero BSS gaps.
    // Zero the entire range first to handle gaps between segments safely.
    unsafe { core::ptr::write_bytes(base_ptr, 0, load_size); }
    let t2 = crate::clock::nanos_since_boot();
    for phdr in segments.iter() {
        if phdr.p_type == PT_LOAD {
            let dst = (base + phdr.p_vaddr) as *mut u8;
            let src = &data[phdr.p_offset as usize..][..phdr.p_filesz as usize];
            unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), dst, phdr.p_filesz as usize); }
        }
    }
    let t3 = crate::clock::nanos_since_boot();

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

    // Count .dynsym entries from GNU hash (avoids parsing section headers)
    let sym_count = if gnu_hash_vaddr != 0 {
        gnu_hash_sym_count(base + gnu_hash_vaddr)
    } else {
        // Fallback: parse section headers
        let mut count = 0;
        if let Some(shdrs) = elf.section_headers() {
            for shdr in shdrs.iter() {
                if shdr.sh_type == SHT_DYNSYM {
                    count = (shdr.sh_size / shdr.sh_entsize.max(24)) as usize;
                    break;
                }
            }
        }
        count
    };

    let dynsym = base + symtab_vaddr;
    let dynstr = base + strtab_vaddr;

    // Apply R_X86_64_RELATIVE relocations using DT_RELA/DT_JMPREL from PT_DYNAMIC
    // (much faster than parsing section headers for .rela.dyn/.rela.plt)
    let entry_size = 24u64;
    let mut reloc_count = 0u64;
    for &(rela_addr, rela_sz) in &[(base + rela_vaddr, rela_size), (base + jmprel_vaddr, jmprel_size)] {
        if rela_sz == 0 { continue; }
        let num = rela_sz / entry_size;
        for i in 0..num {
            let rela_ptr = (rela_addr + i * entry_size) as *const u8;
            let r_offset = unsafe { *(rela_ptr as *const u64) };
            let r_info = unsafe { *(rela_ptr.add(8) as *const u64) };
            let r_addend = unsafe { *(rela_ptr.add(16) as *const i64) };
            let r_type = (r_info & 0xFFFF_FFFF) as u32;
            if r_type == R_X86_64_RELATIVE {
                let target = (base + r_offset) as *mut u64;
                let value = (base as i64 + r_addend) as u64;
                unsafe { *target = value; }
                reloc_count += 1;
            }
        }
    }

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

    let t4 = crate::clock::nanos_since_boot();
    log!("dlopen: {}MB alloc={}ms zero={}ms copy={}ms reloc={}ms ({} relocs, {} syms)",
        load_size / (1024*1024),
        (t1 - t0) / 1_000_000, (t2 - t1) / 1_000_000, (t3 - t2) / 1_000_000,
        (t4 - t3) / 1_000_000, reloc_count, sym_count);

    Ok((LoadedLib { memory: LibMemory::Owned(alloc), base, dynsym, dynstr, dynstr_size: strtab_size, sym_count,
        tls_template, tls_filesz, tls_memsz,
        rela_addr: base + rela_vaddr, rela_size,
        jmprel_addr: base + jmprel_vaddr, jmprel_size,
        gnu_hash: if gnu_hash_vaddr != 0 { base + gnu_hash_vaddr } else { 0 } }, rw_vaddr))
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
                        let target = lib.rw_write_ptr::<u64>(lib.base + r_offset);
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
        let t_load0 = crate::clock::nanos_since_boot();

        let lib_path = alloc::format!("{}/{}", exe_dir, lib_name);

        // Check the shared library cache first
        let cache_idx = {
            let cache = SO_CACHE.lock();
            cache.iter().position(|(path, _)| path == &lib_path)
        };
        let cached_lib = cache_idx.and_then(|idx| {
            let cache = SO_CACHE.lock();
            clone_from_cache(&cache[idx].1)
        });

        if let Some(lib) = cached_lib {
            libs.push(lib);
            continue;
        }

        let so_data = match read_file(&lib_path) {
            Ok(d) => d,
            Err(_) => {
                let fallback = alloc::format!("/lib/{}", lib_name);
                match read_file(&fallback) {
                    Ok(d) => d,
                    Err(e) => return Err(alloc::format!("{}: {}", lib_name, e)),
                }
            }
        };
        let t_load1 = crate::clock::nanos_since_boot();

        match load_shared_lib(&so_data) {
            Ok((lib, rw_vaddr)) => {
                let t_load2 = crate::clock::nanos_since_boot();
                let alloc_ptr = match &lib.memory {
                    LibMemory::Owned(a) => a.ptr() as u64,
                    LibMemory::Shared { shared_addr, .. } => *shared_addr,
                };
                log!("dynamic: loaded {} at {:#x} ({} syms, read={}ms load={}ms)",
                    lib_name, alloc_ptr, lib.sym_count,
                    (t_load1 - t_load0) / 1_000_000, (t_load2 - t_load1) / 1_000_000);

                // Cache this library for future loads
                cache_loaded_lib(&lib_path, &lib, rw_vaddr);

                libs.push(lib);
            }
            Err(e) => return Err(alloc::format!("failed to load {}: {}", lib_name, e)),
        }
    }

    // Apply RELATIVE and GLOB_DAT relocations for the executable.
    // Use raw file data (not loaded image) because the .rela section may not be
    // within any PT_LOAD segment for executables.
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

    // Build a hash map of executable symbols for O(1) lookup.
    // Without this, each relocation does an O(n) linear scan of the symbol table,
    // which is O(n²) total for 200k+ relocations × 150k+ symbols.
    let t_reloc0 = crate::clock::nanos_since_boot();
    let mut exe_sym_map: hashbrown::HashMap<&str, u64> = hashbrown::HashMap::new();
    if let Ok(Some((symtab, strtab))) = elf.symbol_table() {
        for sym in symtab.iter() {
            if sym.st_shndx == 0 { continue; }
            if let Ok(sym_name) = strtab.get(sym.st_name as usize) {
                exe_sym_map.insert(sym_name, base + sym.st_value);
            }
        }
    }

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

                    // Search: executable first (O(1) hash lookup), then other libraries
                    let resolved = exe_sym_map.get(sym_name).copied()
                        .or_else(|| libs.iter().find_map(|other| gnu_dlsym(other, sym_name)));

                    if let Some(addr) = resolved {
                        let target = lib.rw_write_ptr::<u64>(lib.base + r_offset);
                        unsafe { *target = addr; }
                        if sym_name == "main" {
                            log!("dynamic: resolved main -> {:#x}", addr);
                        }
                    } else {
                        log!("dynamic: lib unresolved symbol: {}", sym_name);
                    }
                }
                8 /* R_X86_64_RELATIVE */ => {
                    let target = lib.rw_write_ptr::<u64>(lib.base + r_offset);
                    unsafe { *target = (lib.base as i64 + r_addend) as u64; }
                }
                _ => {}
            }
        }
        } // for sections
    }

    let t_reloc1 = crate::clock::nanos_since_boot();
    log!("dynamic: lib relocations resolved in {}ms ({} exe syms hashed)",
        (t_reloc1 - t_reloc0) / 1_000_000, exe_sym_map.len());

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
                let target = lib.rw_write_ptr::<u64>(lib.base + r_offset);
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
                let target = lib.rw_write_ptr::<i32>(lib.base + r_offset);
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
