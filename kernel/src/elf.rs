use alloc::string::String;
use alloc::vec::Vec;

use crate::arch::paging::{self, PAGE_2M};
use crate::log;
use crate::process::OwnedAlloc;
use crate::sync::Lock;
use elf::ElfBytes;
use elf::endian::AnyEndian;
use elf::file::{parse_ident, FileHeader};
use elf::segment::{ProgramHeader, SegmentTable};
use elf::dynamic::DynamicTable;
use elf::parse::ParseAt;
use elf::abi::{
    PT_LOAD, PT_TLS, PT_DYNAMIC, ET_DYN, EM_X86_64, R_X86_64_RELATIVE,
    DT_SYMTAB, DT_STRTAB, DT_STRSZ, DT_NULL, SHT_DYNSYM, EI_NIDENT,
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
        /// Private copy of the RW segment pages.
        rw_alloc: OwnedAlloc,
        /// Start address of the cached allocation.
        cached_addr: u64,
        /// Total size of the cached allocation.
        cached_size: usize,
        /// 2MB-aligned offset within cached alloc where private RW region starts.
        rw_offset: usize,
        /// 2MB-aligned size of the private RW region.
        rw_size: usize,
        /// Delta to translate cached RW addresses to private physical addresses.
        /// kernel_write_addr = cached_addr + rw_delta
        rw_delta: i64,
    },
}

// ── Pre-scanned relocation data ──────────────────────────────────────────

/// Pre-scanned non-RELATIVE relocation entries from a shared library.
/// Avoids iterating 211K+ entries (99.5% RELATIVE) on every clone.
#[derive(Clone)]
pub struct CachedRelocs {
    /// GLOB_DAT and JUMP_SLOT: (offset_from_base, r_sym)
    pub bind: alloc::vec::Vec<(u64, u32)>,
    /// TPOFF64: (offset_from_base, r_sym, r_addend)
    pub tpoff64: alloc::vec::Vec<(u64, u32, i64)>,
    /// TPOFF32: (offset_from_base, r_sym, r_addend)
    pub tpoff32: alloc::vec::Vec<(u64, u32, i64)>,
}

/// Scan rela/jmprel tables and extract non-RELATIVE entries.
fn prescan_relocs(rela_addr: u64, rela_size: u64, jmprel_addr: u64, jmprel_size: u64) -> CachedRelocs {
    let mut relocs = CachedRelocs {
        bind: alloc::vec::Vec::new(),
        tpoff64: alloc::vec::Vec::new(),
        tpoff32: alloc::vec::Vec::new(),
    };
    let entry_size = 24u64;
    for (addr, size) in [(rela_addr, rela_size), (jmprel_addr, jmprel_size)] {
        if size == 0 { continue; }
        let count = size / entry_size;
        for i in 0..count {
            let ptr = (addr + i * entry_size) as *const u8;
            let r_offset = unsafe { *(ptr as *const u64) };
            let r_info = unsafe { *(ptr.add(8) as *const u64) };
            let r_addend = unsafe { *(ptr.add(16) as *const i64) };
            let r_type = (r_info & 0xFFFF_FFFF) as u32;
            let r_sym = (r_info >> 32) as u32;
            match r_type {
                6 | 7 => relocs.bind.push((r_offset, r_sym)),
                R_X86_64_TPOFF64 => relocs.tpoff64.push((r_offset, r_sym, r_addend)),
                R_X86_64_TPOFF32 => relocs.tpoff32.push((r_offset, r_sym, r_addend)),
                _ => {}
            }
        }
    }
    relocs
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
    /// 2MB-aligned offset within alloc where the private RW region starts.
    rw_offset: usize,
    /// 2MB-aligned size of the private RW region.
    rw_size: usize,
    /// Metadata offsets relative to base.
    dynsym_off: u64,
    dynstr_off: u64,
    dynstr_size: u64,
    sym_count: usize,
    tls_vaddr: u64,
    tls_filesz: usize,
    tls_memsz: usize,
    tls_align: usize,
    rela_off: u64,
    rela_size: u64,
    jmprel_off: u64,
    jmprel_size: u64,
    gnu_hash_off: u64,
    /// Pre-scanned non-RELATIVE relocations for fast cloning.
    relocs: CachedRelocs,
}

static SO_CACHE: Lock<alloc::vec::Vec<(String, CachedLib)>> = Lock::new(alloc::vec::Vec::new());

/// Store a loaded library in the cache for future reuse.
/// Takes ownership of the library's allocation (transferring it to the cache).
/// Returns a new `LoadedLib` in `Shared` mode with private RW pages.
/// `rw_vaddr` is the start vaddr of writable PT_LOAD segments, `rw_end_vaddr` is the end.
fn cache_loaded_lib(path: &str, lib: LoadedLib, rw_vaddr: u64, rw_end_vaddr: u64) -> LoadedLib {
    let LoadedLib {
        memory, base, dynsym, dynstr, dynstr_size, sym_count,
        tls_template, tls_filesz, tls_memsz, tls_align,
        rela_addr, rela_size, jmprel_addr, jmprel_size, gnu_hash,
        cached_relocs: _,
    } = lib;

    let alloc = match memory {
        LibMemory::Owned(a) => a,
        other => return LoadedLib {
            memory: other, base, dynsym, dynstr, dynstr_size, sym_count,
            tls_template, tls_filesz, tls_memsz, tls_align,
            rela_addr, rela_size, jmprel_addr, jmprel_size, gnu_hash,
            cached_relocs: None,
        },
    };
    let size = alloc.size();
    let alloc_ptr = alloc.ptr();
    let vaddr_min = alloc_ptr as u64 - base;

    // Compute the 2MB-aligned RW region within the allocation.
    let rw_start_in_alloc = (base + rw_vaddr) as usize - alloc_ptr as usize;
    let rw_end_in_alloc = (base + rw_end_vaddr) as usize - alloc_ptr as usize;
    let rw_offset = rw_start_in_alloc & !(PAGE_2M as usize - 1);
    let rw_size = paging::align_2m(rw_end_in_alloc) - rw_offset;

    // Pre-scan relocs from the allocation (which stays in place as the cache).
    let relocs = prescan_relocs(rela_addr, rela_size, jmprel_addr, jmprel_size);
    log!("dlopen: cached {} with {} bind + {} tpoff64 + {} tpoff32 pre-scanned relocs",
        path, relocs.bind.len(), relocs.tpoff64.len(), relocs.tpoff32.len());

    // Allocate private RW pages for the first user.
    let cached_addr = alloc_ptr as u64;
    let rw_alloc = match OwnedAlloc::new_uninit(rw_size, PAGE_2M as usize) {
        Some(a) => a,
        None => {
            return LoadedLib {
                memory: LibMemory::Owned(alloc), base, dynsym, dynstr, dynstr_size, sym_count,
                tls_template, tls_filesz, tls_memsz, tls_align,
                rela_addr, rela_size, jmprel_addr, jmprel_size, gnu_hash,
                cached_relocs: None,
            };
        }
    };
    let src = unsafe { alloc_ptr.add(rw_offset) };
    unsafe { core::ptr::copy_nonoverlapping(src, rw_alloc.ptr(), rw_size); }
    let rw_delta = rw_alloc.ptr() as i64 - (cached_addr as i64 + rw_offset as i64);

    let cached = CachedLib {
        alloc,
        alloc_size: size,
        vaddr_min,
        rw_offset,
        rw_size,
        dynsym_off: dynsym - base,
        dynstr_off: dynstr - base,
        dynstr_size,
        sym_count,
        tls_vaddr: if tls_memsz > 0 { tls_template - base } else { 0 },
        tls_filesz,
        tls_memsz,
        tls_align,
        rela_off: rela_addr - base,
        rela_size,
        jmprel_off: jmprel_addr - base,
        jmprel_size,
        gnu_hash_off: if gnu_hash != 0 { gnu_hash - base } else { 0 },
        relocs: relocs.clone(),
    };
    SO_CACHE.lock().push((String::from(path), cached));

    LoadedLib {
        memory: LibMemory::Shared {
            rw_alloc, cached_addr, cached_size: size, rw_offset, rw_size, rw_delta,
        },
        base, dynsym, dynstr, dynstr_size, sym_count,
        tls_template, tls_filesz, tls_memsz, tls_align,
        rela_addr, rela_size, jmprel_addr, jmprel_size, gnu_hash,
        cached_relocs: Some(relocs),
    }
}

/// Public wrapper for caching from syscall path.
pub fn cache_loaded_lib_pub(path: &str, lib: LoadedLib, rw_vaddr: u64, rw_end_vaddr: u64) -> LoadedLib {
    cache_loaded_lib(path, lib, rw_vaddr, rw_end_vaddr)
}

/// Try to clone a library from the cache by path. Returns None if not cached.
pub fn try_clone_cached(path: &str) -> Option<LoadedLib> {
    let cache = SO_CACHE.lock();
    let idx = cache.iter().position(|(p, _)| p == path)?;
    clone_from_cache(&cache[idx].1)
}

/// Clone a LoadedLib from a cached image — shares RO pages, copies only RW pages.
/// Base address stays the same as the cache so RELATIVE relocations need zero fixup.
fn clone_from_cache(cached: &CachedLib) -> Option<LoadedLib> {
    let t0 = crate::clock::nanos_since_boot();

    let cached_ptr = cached.alloc.ptr() as u64;
    let base = cached_ptr - cached.vaddr_min;

    // Allocate and copy only the private (RW) portion
    let rw_alloc = OwnedAlloc::new_uninit(cached.rw_size, PAGE_2M as usize)?;
    let src = unsafe { cached.alloc.ptr().add(cached.rw_offset) };
    unsafe { core::ptr::copy_nonoverlapping(src, rw_alloc.ptr(), cached.rw_size); }

    let t1 = crate::clock::nanos_since_boot();
    let rw_delta = rw_alloc.ptr() as i64 - (cached_ptr as i64 + cached.rw_offset as i64);

    log!("dlopen: cache hit (shared), {}MB total, {}MB private RW, copy={}ms",
        cached.alloc_size / (1024*1024), cached.rw_size / (1024*1024),
        (t1 - t0) / 1_000_000);

    Some(LoadedLib {
        memory: LibMemory::Shared {
            rw_alloc,
            cached_addr: cached_ptr,
            cached_size: cached.alloc_size,
            rw_offset: cached.rw_offset,
            rw_size: cached.rw_size,
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
        tls_align: cached.tls_align,
        rela_addr: base + cached.rela_off,
        rela_size: cached.rela_size,
        jmprel_addr: base + cached.jmprel_off,
        jmprel_size: cached.jmprel_size,
        gnu_hash: if cached.gnu_hash_off != 0 { base + cached.gnu_hash_off } else { 0 },
        cached_relocs: Some(cached.relocs.clone()),
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

// ── Demand-paged ELF layout ─────────────────────────────────────────────

/// PT_LOAD segment info extracted from ELF headers.
pub struct ElfSegment {
    /// Virtual address (relative to ELF base).
    pub vaddr: u64,
    /// Size in memory.
    pub memsz: u64,
    /// Size in file (may be < memsz for BSS).
    pub filesz: u64,
    /// Offset within the file.
    pub file_offset: u64,
    /// Whether this segment is writable.
    pub writable: bool,
}

/// ELF layout parsed from program headers only — no data read beyond the first few KB.
pub struct ElfLayout {
    pub entry_vaddr: u64,
    pub vaddr_min: u64,
    pub vaddr_max: u64,
    pub segments: Vec<ElfSegment>,
    pub tls_vaddr: u64,
    pub tls_filesz: usize,
    pub tls_memsz: usize,
    pub tls_align: usize,
    /// PT_DYNAMIC segment location (file_offset, size), or None if absent.
    pub dynamic: Option<(u64, u64)>,
    /// Section header table location (e_shoff, e_shnum, e_shentsize) for loading symbols.
    pub section_headers: Option<(u64, u16, u16)>,
}

/// Parsed DT_* entries from a PT_DYNAMIC segment.
pub struct DynamicInfo {
    pub rela_vaddr: u64,
    pub rela_size: u64,
    pub jmprel_vaddr: u64,
    pub jmprel_size: u64,
    pub strtab_vaddr: u64,
    pub symtab_vaddr: u64,
    pub strsz: u64,
    pub gnu_hash_vaddr: u64,
    pub needed_strtab_offsets: Vec<u64>,
}

impl DynamicInfo {
    pub fn empty() -> Self {
        Self {
            rela_vaddr: 0, rela_size: 0,
            jmprel_vaddr: 0, jmprel_size: 0,
            strtab_vaddr: 0, symtab_vaddr: 0, strsz: 0,
            gnu_hash_vaddr: 0,
            needed_strtab_offsets: Vec::new(),
        }
    }
}

/// Parse DT_* entries from raw PT_DYNAMIC segment data.
pub fn parse_dynamic(data: &[u8]) -> DynamicInfo {
    let mut info = DynamicInfo::empty();
    let table = DynamicTable::new(AnyEndian::Little, elf::file::Class::ELF64, data);
    for entry in table.iter() {
        match entry.d_tag {
            7 /* DT_RELA */      => info.rela_vaddr = entry.d_val(),
            8 /* DT_RELASZ */    => info.rela_size = entry.d_val(),
            23 /* DT_JMPREL */   => info.jmprel_vaddr = entry.d_val(),
            2 /* DT_PLTRELSZ */  => info.jmprel_size = entry.d_val(),
            1 /* DT_NEEDED */    => info.needed_strtab_offsets.push(entry.d_val()),
            5 /* DT_STRTAB */    => info.strtab_vaddr = entry.d_val(),
            6 /* DT_SYMTAB */    => info.symtab_vaddr = entry.d_val(),
            10 /* DT_STRSZ */   => info.strsz = entry.d_val(),
            d if d == 0x6ffffef5u64 as i64 /* DT_GNU_HASH */ => info.gnu_hash_vaddr = entry.d_val(),
            0 /* DT_NULL */      => break,
            _ => {}
        }
    }
    info
}

/// Parse ELF headers to extract layout without reading data.
/// Only reads the first few KB of the file (headers + program headers).
pub fn parse_layout(data: &[u8]) -> Result<ElfLayout, &'static str> {
    // Parse ELF header and program headers directly from raw bytes.
    // Unlike ElfBytes::minimal_parse(), this does NOT validate section headers,
    // so it works with a partial buffer (e.g. first 4KB of the file).
    if data.len() < EI_NIDENT + 48 { return Err("ELF: buffer too small for header"); }
    let ident = parse_ident::<AnyEndian>(&data[..EI_NIDENT]).map_err(|_| "ELF: bad ident")?;
    let ehdr = FileHeader::parse_tail(ident, &data[EI_NIDENT..]).map_err(|_| "ELF: bad header")?;
    if ehdr.e_type != ET_DYN { return Err("ELF: not PIE (expected ET_DYN)"); }
    if ehdr.e_machine != EM_X86_64 { return Err("ELF: not x86_64"); }
    if ehdr.e_phnum == 0 { return Err("ELF: no program headers"); }

    let ph_offset = ehdr.e_phoff as usize;
    let ph_entry_size = ProgramHeader::size_for(ehdr.class);
    let ph_end = ph_offset + ph_entry_size * ehdr.e_phnum as usize;
    if ph_end > data.len() { return Err("ELF: program headers extend beyond buffer"); }
    let phdrs = SegmentTable::new(ehdr.endianness, ehdr.class, &data[ph_offset..ph_end]);

    let mut segments = Vec::new();
    let mut vaddr_min = u64::MAX;
    let mut vaddr_max = 0u64;
    let mut tls_vaddr = 0u64;
    let mut tls_filesz = 0usize;
    let mut tls_memsz = 0usize;
    let mut tls_align = 0usize;
    let mut dynamic = None;

    for phdr in phdrs.iter() {
        match phdr.p_type {
            PT_LOAD => {
                vaddr_min = vaddr_min.min(phdr.p_vaddr);
                vaddr_max = vaddr_max.max(phdr.p_vaddr + phdr.p_memsz);
                segments.push(ElfSegment {
                    vaddr: phdr.p_vaddr,
                    memsz: phdr.p_memsz,
                    filesz: phdr.p_filesz,
                    file_offset: phdr.p_offset,
                    writable: phdr.p_flags & 2 != 0, // PF_W
                });
            }
            PT_TLS => {
                tls_vaddr = phdr.p_vaddr;
                tls_filesz = phdr.p_filesz as usize;
                tls_memsz = phdr.p_memsz as usize;
                tls_align = phdr.p_align as usize;
            }
            PT_DYNAMIC => {
                dynamic = Some((phdr.p_offset, phdr.p_filesz));
            }
            _ => {}
        }
    }

    if segments.is_empty() { return Err("ELF: no loadable segments"); }

    let section_headers = if ehdr.e_shoff != 0 && ehdr.e_shnum > 0 {
        Some((ehdr.e_shoff, ehdr.e_shnum, ehdr.e_shentsize))
    } else {
        None
    };

    Ok(ElfLayout {
        entry_vaddr: ehdr.e_entry,
        vaddr_min, vaddr_max,
        segments,
        tls_vaddr, tls_filesz, tls_memsz, tls_align,
        dynamic,
        section_headers,
    })
}

/// Find `.rela.dyn` section via section headers when PT_DYNAMIC is absent.
/// Returns (file_offset, size) of the relocation section, if found.
pub fn find_rela_dyn_from_sections(
    shdr_data: &[u8],
    shentsize: u16,
    shstrtab_reader: &dyn Fn(u64, usize) -> Vec<u8>,
) -> Option<(u64, u64)> {
    let shent = shentsize as usize;
    let shnum = shdr_data.len() / shent;

    // First, find .shstrtab (section name string table) — usually the last section
    // We need it to look up section names.
    // The shstrtab index is typically e_shstrndx, but we don't have it here.
    // Instead, find a SHT_STRTAB section that contains ".rela.dyn" by trying each.

    // SHT_RELA = 4
    for i in 0..shnum {
        let off = i * shent;
        if off + shent > shdr_data.len() { break; }
        let sh_type = u32::from_le_bytes(shdr_data[off + 4..off + 8].try_into().ok()?);
        if sh_type == 4 { // SHT_RELA
            let sh_offset = u64::from_le_bytes(shdr_data[off + 24..off + 32].try_into().ok()?);
            let sh_size = u64::from_le_bytes(shdr_data[off + 32..off + 40].try_into().ok()?);
            let sh_entsize = u64::from_le_bytes(shdr_data[off + 56..off + 64].try_into().ok()?);
            // R_X86_64 RELA entries are 24 bytes each
            if sh_entsize == 24 && sh_size > 0 {
                // Verify it contains R_X86_64_RELATIVE entries by reading first entry
                let first = shstrtab_reader(sh_offset, 24.min(sh_size as usize));
                if first.len() >= 24 {
                    let info = u64::from_le_bytes(first[8..16].try_into().unwrap());
                    let r_type = (info & 0xFFFFFFFF) as u32;
                    if r_type == R_X86_64_RELATIVE {
                        return Some((sh_offset, sh_size));
                    }
                }
            }
        }
    }
    None
}

/// Pre-computed relocation index for per-page application during demand faulting.
/// All values are pre-computed at spawn time — the fault handler just writes them.
/// Stores u64 writes (RELATIVE, GLOB_DAT, TPOFF64) and i32 writes (TPOFF32) separately.
pub struct RelocationIndex {
    /// Pre-computed u64 writes: (r_offset, value). Sorted by offset after finalize().
    /// RELATIVE: value = base + addend
    /// GLOB_DAT: value = resolved symbol address
    /// TPOFF64: value = tpoff as u64
    entries_u64: Vec<(u64, u64)>,
    /// Pre-computed i32 writes: (r_offset, value). Sorted by offset after finalize().
    /// TPOFF32: value = tpoff as i32
    entries_i32: Vec<(u64, i32)>,
}

impl RelocationIndex {
    pub fn new() -> Self {
        Self { entries_u64: Vec::new(), entries_i32: Vec::new() }
    }

    pub fn add_u64(&mut self, offset: u64, value: u64) {
        self.entries_u64.push((offset, value));
    }

    pub fn add_i32(&mut self, offset: u64, value: i32) {
        self.entries_i32.push((offset, value));
    }

    /// Sort all entries by offset. Must be called after all entries are added.
    pub fn finalize(&mut self) {
        self.entries_u64.sort_unstable_by_key(|&(off, _)| off);
        self.entries_i32.sort_unstable_by_key(|&(off, _)| off);
    }

    /// Apply pre-computed relocations that fall within [page_offset, page_offset + 4096).
    /// `page_ptr` is a writable pointer to the page data.
    pub fn apply_to_page(&self, page_offset: u64, page_ptr: *mut u8) {
        let end_offset = page_offset + 4096;

        // Apply u64 writes
        let start = self.entries_u64.partition_point(|&(off, _)| off < page_offset);
        for &(r_offset, value) in &self.entries_u64[start..] {
            if r_offset >= end_offset { break; }
            let within_page = (r_offset - page_offset) as usize;
            if within_page + 8 <= 4096 {
                unsafe {
                    core::ptr::write_unaligned(page_ptr.add(within_page) as *mut u64, value);
                }
            }
        }

        // Apply i32 writes
        let start = self.entries_i32.partition_point(|&(off, _)| off < page_offset);
        for &(r_offset, value) in &self.entries_i32[start..] {
            if r_offset >= end_offset { break; }
            let within_page = (r_offset - page_offset) as usize;
            if within_page + 4 <= 4096 {
                unsafe {
                    core::ptr::write_unaligned(page_ptr.add(within_page) as *mut i32, value);
                }
            }
        }
    }

    /// Check if any relocations fall within [page_offset, page_offset + 4096).
    pub fn has_relocs_in_page(&self, page_offset: u64) -> bool {
        let end_offset = page_offset + 4096;
        let start_u64 = self.entries_u64.partition_point(|&(off, _)| off < page_offset);
        if start_u64 < self.entries_u64.len() && self.entries_u64[start_u64].0 < end_offset {
            return true;
        }
        let start_i32 = self.entries_i32.partition_point(|&(off, _)| off < page_offset);
        start_i32 < self.entries_i32.len() && self.entries_i32[start_i32].0 < end_offset
    }

    /// Count how many relocations fall within [page_offset, page_offset + 4096).
    pub fn count_in_page(&self, page_offset: u64) -> usize {
        let end_offset = page_offset + 4096;
        let s64 = self.entries_u64.partition_point(|&(off, _)| off < page_offset);
        let e64 = self.entries_u64.partition_point(|&(off, _)| off < end_offset);
        let s32 = self.entries_i32.partition_point(|&(off, _)| off < page_offset);
        let e32 = self.entries_i32.partition_point(|&(off, _)| off < end_offset);
        (e64 - s64) + (e32 - s32)
    }

    pub fn len(&self) -> usize { self.entries_u64.len() + self.entries_i32.len() }
}

/// Categorized relocation entries parsed from raw rela tables.
pub struct ParsedRelaEntries {
    /// R_X86_64_RELATIVE: (r_offset, r_addend)
    pub relative: Vec<(u64, i64)>,
    /// R_X86_64_GLOB_DAT + R_X86_64_JUMP_SLOT: (r_offset, r_sym, r_addend)
    pub glob_dat: Vec<(u64, u32, i64)>,
    /// R_X86_64_TPOFF64: (r_offset, r_sym, r_addend)
    pub tpoff64: Vec<(u64, u32, i64)>,
    /// R_X86_64_TPOFF32: (r_offset, r_sym, r_addend)
    pub tpoff32: Vec<(u64, u32, i64)>,
}

/// Parse raw rela tables into categorized entries by relocation type.
pub fn parse_rela_entries(rela_data: &[u8], jmprel_data: &[u8]) -> ParsedRelaEntries {
    let mut result = ParsedRelaEntries {
        relative: Vec::new(),
        glob_dat: Vec::new(),
        tpoff64: Vec::new(),
        tpoff32: Vec::new(),
    };
    for data in [rela_data, jmprel_data] {
        let count = data.len() / 24;
        for i in 0..count {
            let off = i * 24;
            let r_offset = u64::from_le_bytes(data[off..off + 8].try_into().unwrap());
            let r_info = u64::from_le_bytes(data[off + 8..off + 16].try_into().unwrap());
            let r_addend = i64::from_le_bytes(data[off + 16..off + 24].try_into().unwrap());
            let r_type = (r_info & 0xFFFF_FFFF) as u32;
            let r_sym = (r_info >> 32) as u32;
            match r_type {
                R_X86_64_RELATIVE => result.relative.push((r_offset, r_addend)),
                6 | 7 => result.glob_dat.push((r_offset, r_sym, r_addend)), // GLOB_DAT | JUMP_SLOT
                R_X86_64_TPOFF64 => result.tpoff64.push((r_offset, r_sym, r_addend)),
                R_X86_64_TPOFF32 => result.tpoff32.push((r_offset, r_sym, r_addend)),
                _ => {}
            }
        }
    }
    result
}

/// Compute .dynsym entry count from a GNU hash table stored in a byte slice.
pub fn gnu_hash_sym_count_from_data(data: &[u8]) -> usize {
    if data.len() < 16 { return 0; }
    let nbuckets = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
    let symoffset = u32::from_le_bytes(data[4..8].try_into().unwrap()) as usize;
    let bloom_size = u32::from_le_bytes(data[8..12].try_into().unwrap()) as usize;

    let buckets_start = 16 + bloom_size * 8;
    let chains_start = buckets_start + nbuckets * 4;

    if chains_start > data.len() { return symoffset; }

    let mut max_sym = 0usize;
    for i in 0..nbuckets {
        let off = buckets_start + i * 4;
        if off + 4 > data.len() { return symoffset; }
        let val = u32::from_le_bytes(data[off..off + 4].try_into().unwrap()) as usize;
        if val > max_sym { max_sym = val; }
    }
    if max_sym < symoffset { return symoffset; }

    let mut idx = max_sym - symoffset;
    loop {
        let off = chains_start + idx * 4;
        if off + 4 > data.len() { return symoffset + idx + 1; }
        let entry = u32::from_le_bytes(data[off..off + 4].try_into().unwrap());
        if entry & 1 != 0 { return symoffset + idx + 1; }
        idx += 1;
    }
}

/// Build an exe symbol map from raw .dynsym and .dynstr data.
/// Returns a HashMap of symbol name → runtime address (base + st_value).
pub fn build_exe_sym_map<'a>(
    dynsym_data: &[u8],
    dynstr_data: &'a [u8],
    sym_count: usize,
    base: u64,
) -> hashbrown::HashMap<&'a str, u64> {
    let mut map = hashbrown::HashMap::with_capacity(sym_count);
    for i in 1..sym_count {
        let off = i * 24;
        if off + 24 > dynsym_data.len() { break; }
        let st_name = u32::from_le_bytes(dynsym_data[off..off + 4].try_into().unwrap()) as usize;
        let st_shndx = u16::from_le_bytes(dynsym_data[off + 6..off + 8].try_into().unwrap());
        let st_value = u64::from_le_bytes(dynsym_data[off + 8..off + 16].try_into().unwrap());
        if st_shndx == 0 { continue; }
        if st_name >= dynstr_data.len() { continue; }
        // Find null terminator
        let name_end = dynstr_data[st_name..].iter().position(|&b| b == 0)
            .unwrap_or(dynstr_data.len() - st_name);
        let name = core::str::from_utf8(&dynstr_data[st_name..st_name + name_end]).unwrap_or("");
        if !name.is_empty() {
            map.insert(name, base + st_value);
        }
    }
    map
}

/// Build a symbol map from .symtab section headers when .dynsym has no defined symbols.
/// This is the fallback for PIE executables that don't use --export-dynamic.
pub fn build_symtab_map(
    shdr_data: &[u8],
    shentsize: u16,
    block_map: &[u64],
    base: u64,
) -> Option<hashbrown::HashMap<&'static str, u64>> {
    let shent = shentsize as usize;
    let shnum = shdr_data.len() / shent;

    // Find SHT_SYMTAB and its linked SHT_STRTAB
    let mut symtab_off = 0u64;
    let mut symtab_size = 0u64;
    let mut symtab_link = 0u32;
    let mut found = false;

    for i in 0..shnum {
        let off = i * shent;
        if off + shent > shdr_data.len() { break; }
        let sh_type = u32::from_le_bytes(shdr_data[off + 4..off + 8].try_into().ok()?);
        if sh_type == 2 { // SHT_SYMTAB
            symtab_off = u64::from_le_bytes(shdr_data[off + 24..off + 32].try_into().ok()?);
            symtab_size = u64::from_le_bytes(shdr_data[off + 32..off + 40].try_into().ok()?);
            symtab_link = u32::from_le_bytes(shdr_data[off + 40..off + 44].try_into().ok()?);
            found = true;
            break;
        }
    }
    if !found { return None; }

    // Read linked .strtab
    let link_off = symtab_link as usize * shent;
    if link_off + shent > shdr_data.len() { return None; }
    let strtab_off = u64::from_le_bytes(shdr_data[link_off + 24..link_off + 32].try_into().ok()?);
    let strtab_size = u64::from_le_bytes(shdr_data[link_off + 32..link_off + 40].try_into().ok()?);

    let symtab_data = crate::process::read_file_range(block_map, symtab_off, symtab_size as usize);
    let strtab_data = crate::process::read_file_range(block_map, strtab_off, strtab_size as usize);

    // Leak the strtab data so we can return &'static str references
    let strtab_leaked: &'static [u8] = alloc::vec::Vec::leak(strtab_data);

    let sym_count = symtab_data.len() / 24;
    let mut map = hashbrown::HashMap::with_capacity(sym_count);
    for i in 1..sym_count {
        let off = i * 24;
        if off + 24 > symtab_data.len() { break; }
        let st_name = u32::from_le_bytes(symtab_data[off..off + 4].try_into().unwrap()) as usize;
        let st_info = symtab_data[off + 4];
        let st_shndx = u16::from_le_bytes(symtab_data[off + 6..off + 8].try_into().unwrap());
        let st_value = u64::from_le_bytes(symtab_data[off + 8..off + 16].try_into().unwrap());
        // Only export GLOBAL or WEAK symbols that are defined (st_shndx != 0)
        let bind = st_info >> 4;
        if st_shndx == 0 || (bind != 1 && bind != 2) { continue; } // STB_GLOBAL=1, STB_WEAK=2
        if st_name >= strtab_leaked.len() { continue; }
        let name_end = strtab_leaked[st_name..].iter().position(|&b| b == 0)
            .unwrap_or(strtab_leaked.len() - st_name);
        let name = core::str::from_utf8(&strtab_leaked[st_name..st_name + name_end]).unwrap_or("");
        if !name.is_empty() {
            map.insert(name, base + st_value);
        }
    }
    Some(map)
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
    pub tls_align: usize,
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
    /// Pre-scanned non-RELATIVE relocs (only for cached/shared libs).
    pub cached_relocs: Option<CachedRelocs>,
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
/// Eagerly loads the entire ELF into memory, applies RELATIVE relocations,
/// and parses PT_DYNAMIC for .dynsym/.dynstr symbol tables.
/// Returns (LoadedLib, rw_vaddr, rw_end_vaddr) for RW region tracking.
pub fn load_shared_lib(data: &[u8]) -> Result<(LoadedLib, u64, u64), &'static str> {
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

    // Scan PT_LOAD for address range and writable segment bounds
    let mut vaddr_range: Option<(u64, u64)> = None;
    let mut rw_start: Option<u64> = None;
    let mut rw_end: Option<u64> = None;
    for phdr in segments.iter().filter(|p| p.p_type == PT_LOAD) {
        let lo = phdr.p_vaddr;
        let hi = phdr.p_vaddr + phdr.p_memsz;
        vaddr_range = Some(match vaddr_range {
            None => (lo, hi),
            Some((min, max)) => (min.min(lo), max.max(hi)),
        });
        if phdr.p_flags & 0x2 != 0 { // PF_W
            rw_start = Some(rw_start.map_or(lo, |v: u64| v.min(lo)));
            rw_end = Some(rw_end.map_or(hi, |v: u64| v.max(hi)));
        }
    }
    let (vaddr_min, vaddr_max) = vaddr_range.ok_or("dlopen: no loadable segments")?;
    let rw_vaddr = rw_start.unwrap_or(vaddr_max);
    let rw_end_vaddr = rw_end.unwrap_or(vaddr_max);

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
    let mut tls_align = 0usize;
    for phdr in segments.iter() {
        if phdr.p_type == PT_TLS {
            tls_template = base + phdr.p_vaddr;
            tls_filesz = phdr.p_filesz as usize;
            tls_memsz = phdr.p_memsz as usize;
            tls_align = phdr.p_align as usize;
        }
    }

    let t4 = crate::clock::nanos_since_boot();
    log!("dlopen: {}MB alloc={}ms zero={}ms copy={}ms reloc={}ms ({} relocs, {} syms)",
        load_size / (1024*1024),
        (t1 - t0) / 1_000_000, (t2 - t1) / 1_000_000, (t3 - t2) / 1_000_000,
        (t4 - t3) / 1_000_000, reloc_count, sym_count);

    Ok((LoadedLib { memory: LibMemory::Owned(alloc), base, dynsym, dynstr, dynstr_size: strtab_size, sym_count,
        tls_template, tls_filesz, tls_memsz, tls_align,
        rela_addr: base + rela_vaddr, rela_size,
        jmprel_addr: base + jmprel_vaddr, jmprel_size,
        gnu_hash: if gnu_hash_vaddr != 0 { base + gnu_hash_vaddr } else { 0 },
        cached_relocs: None }, rw_vaddr, rw_end_vaddr))
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
    let mut resolved_count = 0u64;
    let mut unresolved_count = 0u64;

    let resolve_one = |r_offset: u64, r_sym: u32, resolved_count: &mut u64, unresolved_count: &mut u64| {
        let sym_entry = (lib.dynsym + r_sym as u64 * 24) as *const u8;
        let st_name = unsafe { *(sym_entry as *const u32) };
        let sym_name = bounded_cstr(lib.dynstr, st_name as u64, lib.dynstr_size);
        let resolved = other_libs.iter().find_map(|other| gnu_dlsym(other, sym_name));
        if let Some(addr) = resolved {
            let target = lib.rw_write_ptr::<u64>(lib.base + r_offset);
            unsafe { *target = addr; }
            *resolved_count += 1;
        } else {
            if *unresolved_count < 5 {
                log!("dlopen: unresolved: {}", sym_name);
            }
            *unresolved_count += 1;
        }
    };

    if let Some(relocs) = &lib.cached_relocs {
        for &(r_offset, r_sym) in &relocs.bind {
            resolve_one(r_offset, r_sym, &mut resolved_count, &mut unresolved_count);
        }
    } else {
        let entry_size = 24u64;
        for (rela_addr, rela_size) in [(lib.rela_addr, lib.rela_size), (lib.jmprel_addr, lib.jmprel_size)] {
            if rela_size == 0 { continue; }
            let count = rela_size / entry_size;
            for i in 0..count {
                let rela_ptr = (rela_addr + i * entry_size) as *const u8;
                let r_offset = unsafe { *(rela_ptr as *const u64) };
                let r_info = unsafe { *(rela_ptr.add(8) as *const u64) };
                let r_type = (r_info & 0xFFFF_FFFF) as u32;
                let r_sym = (r_info >> 32) as u32;
                if r_type == 6 || r_type == 7 {
                    resolve_one(r_offset, r_sym, &mut resolved_count, &mut unresolved_count);
                }
            }
        }
    }
    log!("dlopen: resolved {} relocs, {} unresolved", resolved_count, unresolved_count);
}

/// Public wrapper for resolve_lib_bind_relocs.
pub fn resolve_lib_bind_relocs_pub(
    lib: &LoadedLib,
    exe_sym_map: &hashbrown::HashMap<&str, u64>,
    libs: &[LoadedLib],
) {
    resolve_lib_bind_relocs(lib, exe_sym_map, libs);
}

/// Public wrapper for gnu_dlsym.
pub fn gnu_dlsym_pub(lib: &LoadedLib, name: &str) -> Option<u64> {
    gnu_dlsym(lib, name)
}

/// Resolve GLOB_DAT/JUMP_SLOT relocations for a single library.
/// Uses pre-scanned reloc data when available (cached libs) to avoid iterating all entries.
fn resolve_lib_bind_relocs(
    lib: &LoadedLib,
    exe_sym_map: &hashbrown::HashMap<&str, u64>,
    libs: &[LoadedLib],
) {
    if let Some(relocs) = &lib.cached_relocs {
        // Fast path: only iterate the pre-scanned GLOB_DAT/JUMP_SLOT entries
        for &(r_offset, r_sym) in &relocs.bind {
            let sym_entry = (lib.dynsym + r_sym as u64 * 24) as *const u8;
            let st_name = unsafe { *(sym_entry as *const u32) };
            let sym_name = bounded_cstr(lib.dynstr, st_name as u64, lib.dynstr_size);
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
    } else {
        // Slow path: scan all relocation entries (fresh load, not cached)
        let entry_size = 24u64;
        for (rela_addr, rela_size) in [(lib.rela_addr, lib.rela_size), (lib.jmprel_addr, lib.jmprel_size)] {
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
                    6 | 7 => {
                        let sym_entry = (lib.dynsym + r_sym as u64 * 24) as *const u8;
                        let st_name = unsafe { *(sym_entry as *const u32) };
                        let sym_name = bounded_cstr(lib.dynstr, st_name as u64, lib.dynstr_size);
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
                    8 => {
                        let target = lib.rw_write_ptr::<u64>(lib.base + r_offset);
                        unsafe { *target = (lib.base as i64 + r_addend) as u64; }
                    }
                    _ => {}
                }
            }
        }
    }
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
    if let Some(relocs) = &lib.cached_relocs {
        // Fast path: use pre-scanned TPOFF entries
        for &(r_offset, r_sym, r_addend) in &relocs.tpoff64 {
            let tpoff = compute_tpoff(lib, r_sym, r_addend, lib_base_offset, total_memsz, tls_info);
            let target = lib.rw_write_ptr::<u64>(lib.base + r_offset);
            unsafe { *target = tpoff as u64; }
        }
        for &(r_offset, r_sym, r_addend) in &relocs.tpoff32 {
            let tpoff = compute_tpoff(lib, r_sym, r_addend, lib_base_offset, total_memsz, tls_info);
            let target = lib.rw_write_ptr::<i32>(lib.base + r_offset);
            unsafe { *target = tpoff as i32; }
        }
        if !relocs.tpoff64.is_empty() || !relocs.tpoff32.is_empty() {
            log!("dlopen: applied {} TPOFF64 + {} TPOFF32 relocs (base_offset={}, total_memsz={})",
                relocs.tpoff64.len(), relocs.tpoff32.len(), lib_base_offset, total_memsz);
        }
    } else {
        // Slow path: scan all relocation entries
        let entry_size = 24u64;
        let mut count64 = 0u64;
        let mut count32 = 0u64;
        for (rela_addr, rela_size) in [(lib.rela_addr, lib.rela_size), (lib.jmprel_addr, lib.jmprel_size)] {
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
                    let tpoff = compute_tpoff(lib, r_sym, r_addend, lib_base_offset, total_memsz, tls_info);
                    let target = lib.rw_write_ptr::<u64>(lib.base + r_offset);
                    unsafe { *target = tpoff as u64; }
                    count64 += 1;
                } else if r_type == R_X86_64_TPOFF32 {
                    let tpoff = compute_tpoff(lib, r_sym, r_addend, lib_base_offset, total_memsz, tls_info);
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
}

/// Compute a TPOFF value for a relocation entry.
fn compute_tpoff(lib: &LoadedLib, r_sym: u32, r_addend: i64, lib_base_offset: usize, total_memsz: usize, tls_info: &TlsModuleInfo) -> i64 {
    if r_sym != 0 {
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
    }
}

/// Public wrapper for tls_dlsym.
pub fn tls_dlsym_pub(lib: &LoadedLib, name: &str) -> Option<u64> {
    tls_dlsym(lib, name)
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
