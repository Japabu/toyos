use alloc::string::String;
use alloc::vec::Vec;

use crate::mm::{PAGE_2M, align_2m, DirectMap, KernelSlice};
use crate::process::PageAlloc;
use crate::UserAddr;
use crate::sync::Lock;
use elf::endian::AnyEndian;
use elf::file::{parse_ident, FileHeader};
use elf::segment::{ProgramHeader, SegmentTable};
use elf::dynamic::DynamicTable;
use elf::parse::ParseAt;
use elf::symbol::Elf64_Sym;
use elf::relocation::Elf64_Rela;
use elf::abi::{
    PT_LOAD, PT_TLS, PT_DYNAMIC, ET_DYN, EM_X86_64, R_X86_64_RELATIVE,
    DT_SYMTAB, DT_STRTAB, DT_STRSZ, DT_NULL, SHT_DYNSYM, EI_NIDENT,
};

const R_X86_64_DTPMOD64: u32 = 16;
const R_X86_64_DTPOFF64: u32 = 17;
const R_X86_64_TPOFF64: u32 = 18;
const R_X86_64_TPOFF32: u32 = 23;

pub const SYM_SIZE: usize = core::mem::size_of::<Elf64_Sym>(); // 24
const RELA_SIZE: u64 = core::mem::size_of::<Elf64_Rela>() as u64; // 24

pub fn read_sym(data: &[u8], index: usize) -> Elf64_Sym {
    let off = index * SYM_SIZE;
    unsafe { core::ptr::read_unaligned(data[off..].as_ptr() as *const Elf64_Sym) }
}

pub fn sym_name<'a>(sym: &Elf64_Sym, strtab: &'a [u8]) -> &'a str {
    let start = sym.st_name as usize;
    if start >= strtab.len() { return ""; }
    let len = strtab[start..].iter().position(|&b| b == 0).unwrap_or(strtab.len() - start);
    core::str::from_utf8(&strtab[start..start + len]).unwrap_or("")
}

fn rela_type(r: &Elf64_Rela) -> u32 { (r.r_info & 0xFFFF_FFFF) as u32 }
fn rela_sym(r: &Elf64_Rela) -> u32 { (r.r_info >> 32) as u32 }

// ── Shared library memory ownership ──────────────────────────────────────

/// Ownership model for a loaded shared library's memory.
pub enum LibMemory {
    /// Fresh load: single allocation owns everything.
    Owned(PageAlloc),
    /// Cloned from cache: RO pages are shared (owned by cache), RW pages are private.
    Shared {
        rw_alloc: PageAlloc,
        /// The cached (shared) allocation this was cloned from.
        cached_image: KernelSlice,
        /// 2MB-aligned offset within cached alloc where private RW region starts.
        rw_offset: usize,
        /// Delta to translate cached addresses to private RW addresses.
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
    /// DTPMOD64: (offset_from_base, r_sym, r_addend) — kernel writes module_id
    pub dtpmod64: alloc::vec::Vec<(u64, u32, i64)>,
    /// DTPOFF64: (offset_from_base, r_sym, r_addend) — kernel writes TLS offset within module
    pub dtpoff64: alloc::vec::Vec<(u64, u32, i64)>,
}

/// Scan rela/jmprel tables and extract non-RELATIVE entries.
fn prescan_relocs(rela: &Option<KernelSlice>, jmprel: &Option<KernelSlice>) -> CachedRelocs {
    let mut relocs = CachedRelocs {
        bind: alloc::vec::Vec::new(),
        tpoff64: alloc::vec::Vec::new(),
        tpoff32: alloc::vec::Vec::new(),
        dtpmod64: alloc::vec::Vec::new(),
        dtpoff64: alloc::vec::Vec::new(),
    };
    for table in [rela, jmprel] {
        let Some(table) = table else { continue };
        let count = table.size() / RELA_SIZE as usize;
        for i in 0..count {
            let rela = unsafe { table.read::<Elf64_Rela>(i * RELA_SIZE as usize) };
            match rela_type(&rela) {
                6 | 7 => relocs.bind.push((rela.r_offset, rela_sym(&rela))),
                R_X86_64_TPOFF64 => relocs.tpoff64.push((rela.r_offset, rela_sym(&rela), rela.r_addend)),
                R_X86_64_TPOFF32 => relocs.tpoff32.push((rela.r_offset, rela_sym(&rela), rela.r_addend)),
                R_X86_64_DTPMOD64 => relocs.dtpmod64.push((rela.r_offset, rela_sym(&rela), rela.r_addend)),
                R_X86_64_DTPOFF64 => relocs.dtpoff64.push((rela.r_offset, rela_sym(&rela), rela.r_addend)),
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
    alloc: PageAlloc,
    image: KernelSlice,
    vaddr_min: u64,
    rw_offset: usize,
    rw_size: usize,
    dynsym: Option<KernelSlice>,
    dynstr: Option<KernelSlice>,
    sym_count: usize,
    tls_template: Option<KernelSlice>,
    tls_memsz: usize,
    tls_align: usize,
    rela: Option<KernelSlice>,
    jmprel: Option<KernelSlice>,
    gnu_hash: Option<KernelSlice>,
    relocs: CachedRelocs,
    eh_frame_hdr_vaddr: u64,
    eh_frame_hdr_size: u64,
    init_array_vaddr: u64,
    init_array_size: u64,
    user_end: u64,
}

static SO_CACHE: Lock<alloc::vec::Vec<(String, CachedLib)>> = Lock::new(alloc::vec::Vec::new());

/// Store a loaded library in the cache for future reuse.
/// Takes ownership of the library's allocation (transferring it to the cache).
/// Returns a new `LoadedLib` in `Shared` mode with private RW pages.
/// `rw_vaddr` is the start vaddr of writable PT_LOAD segments, `rw_end_vaddr` is the end.
fn cache_loaded_lib(path: &str, lib: LoadedLib, rw_vaddr: u64, rw_end_vaddr: u64) -> LoadedLib {
    let alloc = match lib.memory {
        LibMemory::Owned(a) => a,
        _ => return lib,
    };
    let alloc_ptr = alloc.ptr();
    let vaddr_min = alloc_ptr as u64 - DirectMap::from_phys(lib.phys_base).as_ptr::<u8>() as u64;

    // Compute the 2MB-aligned RW region within the allocation.
    let rw_start_in_alloc = rw_vaddr as usize - vaddr_min as usize;
    let rw_end_in_alloc = rw_end_vaddr as usize - vaddr_min as usize;
    let rw_offset = rw_start_in_alloc & !(PAGE_2M as usize - 1);
    let rw_size = align_2m(rw_end_in_alloc) - rw_offset;

    let relocs = prescan_relocs(&lib.rela, &lib.jmprel);
    log!("dlopen: cached {} with {} bind + {} tpoff64 + {} tpoff32 + {} dtpmod64 + {} dtpoff64 pre-scanned relocs",
        path, relocs.bind.len(), relocs.tpoff64.len(), relocs.tpoff32.len(),
        relocs.dtpmod64.len(), relocs.dtpoff64.len());

    let rw_alloc = match PageAlloc::new(rw_size) {
        Some(a) => a,
        None => {
            return LoadedLib { memory: LibMemory::Owned(alloc), ..lib };
        }
    };
    let src = unsafe { alloc_ptr.add(rw_offset) };
    unsafe { core::ptr::copy_nonoverlapping(src, rw_alloc.ptr(), rw_size); }
    let rw_delta = rw_alloc.ptr() as i64 - (alloc_ptr as i64 + rw_offset as i64);

    let cached = CachedLib {
        alloc,
        image: lib.image,
        vaddr_min,
        rw_offset,
        rw_size,
        dynsym: lib.dynsym,
        dynstr: lib.dynstr,
        sym_count: lib.sym_count,
        tls_template: lib.tls_template,
        tls_memsz: lib.tls_memsz,
        tls_align: lib.tls_align,
        rela: lib.rela,
        jmprel: lib.jmprel,
        gnu_hash: lib.gnu_hash,
        relocs: relocs.clone(),
        eh_frame_hdr_vaddr: lib.eh_frame_hdr_vaddr,
        eh_frame_hdr_size: lib.eh_frame_hdr_size,
        init_array_vaddr: lib.init_array_vaddr,
        init_array_size: lib.init_array_size,
        user_end: lib.user_end,
    };
    SO_CACHE.lock().push((String::from(path), cached));

    LoadedLib {
        memory: LibMemory::Shared {
            rw_alloc, cached_image: lib.image, rw_offset, rw_delta,
        },
        user_base: lib.user_base, phys_base: lib.phys_base, image: lib.image,
        dynsym: lib.dynsym, dynstr: lib.dynstr, sym_count: lib.sym_count,
        tls_template: lib.tls_template, tls_memsz: lib.tls_memsz, tls_align: lib.tls_align,
        rela: lib.rela, jmprel: lib.jmprel, gnu_hash: lib.gnu_hash,
        cached_relocs: Some(relocs),
        eh_frame_hdr_vaddr: lib.eh_frame_hdr_vaddr, eh_frame_hdr_size: lib.eh_frame_hdr_size,
        init_array_vaddr: lib.init_array_vaddr, init_array_size: lib.init_array_size,
        user_end: lib.user_end,
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

    let rw_alloc = PageAlloc::new(cached.rw_size)?;
    let src = unsafe { cached.alloc.ptr().add(cached.rw_offset) };
    unsafe { core::ptr::copy_nonoverlapping(src, rw_alloc.ptr(), cached.rw_size); }

    let t1 = crate::clock::nanos_since_boot();
    let rw_delta = rw_alloc.ptr() as i64 - (cached.alloc.ptr() as i64 + cached.rw_offset as i64);
    let phys_base = cached.image.phys();

    log!("dlopen: cache hit (shared), base={:#x} {}MB total, {}MB private RW, copy={}ms",
        phys_base, cached.image.size() / (1024*1024), cached.rw_size / (1024*1024),
        (t1 - t0) / 1_000_000);

    Some(LoadedLib {
        memory: LibMemory::Shared {
            rw_alloc,
            cached_image: cached.image,
            rw_offset: cached.rw_offset,
            rw_delta,
        },
        user_base: UserAddr::new(phys_base),
        phys_base,
        image: cached.image,
        dynsym: cached.dynsym,
        dynstr: cached.dynstr,
        sym_count: cached.sym_count,
        tls_template: cached.tls_template,
        tls_memsz: cached.tls_memsz,
        tls_align: cached.tls_align,
        rela: cached.rela,
        jmprel: cached.jmprel,
        gnu_hash: cached.gnu_hash,
        cached_relocs: Some(cached.relocs.clone()),
        eh_frame_hdr_vaddr: cached.eh_frame_hdr_vaddr,
        eh_frame_hdr_size: cached.eh_frame_hdr_size,
        init_array_vaddr: cached.init_array_vaddr,
        init_array_size: cached.init_array_size,
        user_end: cached.user_end,
    })
}

/// Derive total symbol count from a GNU hash table.
/// The table is: [nbuckets, symoffset, bloom_size, bloom_shift, bloom[], buckets[], chain[]]
/// Each bucket holds the lowest symbol index; each chain entry's bit 0 marks the end of a chain.
fn gnu_hash_sym_count(table: &KernelSlice) -> usize {
    let nbuckets = unsafe { table.read::<u32>(0) } as usize;
    let symoffset = unsafe { table.read::<u32>(4) } as usize;
    let bloom_size = unsafe { table.read::<u32>(8) } as usize;
    let buckets_off = 16 + bloom_size * 8;
    let mut max_sym = 0usize;
    for i in 0..nbuckets {
        let val = unsafe { table.read::<u32>(buckets_off + i * 4) } as usize;
        if val > max_sym { max_sym = val; }
    }
    if max_sym < symoffset { return symoffset; }
    let chains_off = buckets_off + nbuckets * 4;
    let mut idx = max_sym - symoffset;
    loop {
        let entry = unsafe { table.read::<u32>(chains_off + idx * 4) };
        if entry & 1 != 0 { return symoffset + idx + 1; }
        idx += 1;
    }
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
    /// PT_DYNAMIC segment location (file_offset, vaddr, size), or None if absent.
    pub dynamic: Option<(u64, u64, u64)>,
    /// Section header table location (e_shoff, e_shnum, e_shentsize) for loading symbols.
    pub section_headers: Option<(u64, u16, u16)>,
    /// PT_GNU_EH_FRAME segment: (vaddr, memsz). Used for DWARF unwinding.
    pub eh_frame_hdr: Option<(u64, u64)>,
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
    let mut eh_frame_hdr = None;

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
                dynamic = Some((phdr.p_offset, phdr.p_vaddr, phdr.p_filesz));
            }
            0x6474e550 /* PT_GNU_EH_FRAME */ => {
                eh_frame_hdr = Some((phdr.p_vaddr, phdr.p_memsz));
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
        eh_frame_hdr,
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
    pub fn apply_to_page(&self, page_offset: u64, page_ptr: *mut u8) -> usize {
        let end_offset = page_offset + 4096;
        let mut count = 0usize;

        // Apply u64 writes
        let start = self.entries_u64.partition_point(|&(off, _)| off < page_offset);
        for &(r_offset, value) in &self.entries_u64[start..] {
            if r_offset >= end_offset { break; }
            let within_page = (r_offset - page_offset) as usize;
            if within_page + 8 <= 4096 {
                unsafe {
                    core::ptr::write_unaligned(page_ptr.add(within_page) as *mut u64, value);
                }
                count += 1;
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
                count += 1;
            }
        }
        count
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
    base: UserAddr,
) -> hashbrown::HashMap<&'a str, UserAddr> {
    let mut map = hashbrown::HashMap::with_capacity(sym_count);
    for i in 1..sym_count {
        if (i + 1) * SYM_SIZE > dynsym_data.len() { break; }
        let sym = read_sym(dynsym_data, i);
        if sym.st_shndx == 0 { continue; }
        let name = sym_name(&sym, dynstr_data);
        if !name.is_empty() {
            map.insert(name, base + sym.st_value);
        }
    }
    map
}

/// Build a symbol map from .symtab section headers when .dynsym has no defined symbols.
/// This is the fallback for PIE executables that don't use --export-dynamic.
pub fn build_symtab_map(
    shdr_data: &[u8],
    shentsize: u16,
    backing: &dyn crate::file_backing::FileBacking,
    base: UserAddr,
) -> Option<hashbrown::HashMap<&'static str, UserAddr>> {
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

    let symtab_data = crate::process::read_file_range(backing, symtab_off, symtab_size as usize);
    let strtab_data = crate::process::read_file_range(backing, strtab_off, strtab_size as usize);

    // Leak the strtab data so we can return &'static str references
    let strtab_leaked: &'static [u8] = alloc::vec::Vec::leak(strtab_data);

    let sym_count = symtab_data.len() / SYM_SIZE;
    let mut map = hashbrown::HashMap::with_capacity(sym_count);
    for i in 1..sym_count {
        let sym = read_sym(&symtab_data, i);
        // Only export GLOBAL or WEAK symbols that are defined (st_shndx != 0)
        let bind = sym.st_info >> 4;
        if sym.st_shndx == 0 || (bind != 1 && bind != 2) { continue; }
        let name = sym_name(&sym, strtab_leaked);
        if !name.is_empty() {
            map.insert(name, base + sym.st_value);
        }
    }
    Some(map)
}

// ── Dynamic linking ──────────────────────────────────────────────────────

pub struct LoadedLib {
    pub memory: LibMemory,
    pub user_base: UserAddr,
    /// Physical base address for page table mappings.
    pub phys_base: u64,
    /// Bounds-checked view of the entire loaded image.
    pub image: KernelSlice,
    pub dynsym: Option<KernelSlice>,
    pub dynstr: Option<KernelSlice>,
    pub sym_count: usize,
    pub tls_template: Option<KernelSlice>,
    pub tls_memsz: usize,
    pub tls_align: usize,
    pub rela: Option<KernelSlice>,
    pub jmprel: Option<KernelSlice>,
    pub gnu_hash: Option<KernelSlice>,
    pub cached_relocs: Option<CachedRelocs>,
    /// .eh_frame_hdr vaddr (relative to module base, from PT_GNU_EH_FRAME).
    pub eh_frame_hdr_vaddr: u64,
    /// .eh_frame_hdr size.
    pub eh_frame_hdr_size: u64,
    /// .init_array vaddr (relative to ELF base, from DT_INIT_ARRAY).
    pub init_array_vaddr: u64,
    /// .init_array size in bytes (each entry is 8 bytes on x86_64).
    pub init_array_size: u64,
    /// Virtual address extent (user_base + vaddr_max).
    pub user_end: u64,
}

impl LoadedLib {
    fn sym(&self, i: usize) -> Elf64_Sym {
        let dynsym = self.dynsym.as_ref().expect("no dynsym");
        unsafe { dynsym.read::<Elf64_Sym>(i * SYM_SIZE) }
    }

    fn sym_name(&self, i: usize) -> &str {
        let dynstr = self.dynstr.as_ref().expect("no dynstr");
        let st_name = self.sym(i).st_name as usize;
        if st_name >= dynstr.size() { return ""; }
        let ptr = dynstr.ptr_at(st_name);
        let remaining = dynstr.size() - st_name;
        let mut len = 0;
        while len < remaining {
            if unsafe { *ptr.add(len) } == 0 { break; }
            len += 1;
        }
        let bytes = unsafe { core::slice::from_raw_parts(ptr, len) };
        core::str::from_utf8(bytes).unwrap_or("")
    }

    pub fn tls_filesz(&self) -> usize {
        self.tls_template.as_ref().map_or(0, |s| s.size())
    }

    /// Write a value at a byte offset within this library's kernel mapping.
    pub unsafe fn write_at<T: Copy>(&self, offset: u64, value: T) {
        match &self.memory {
            LibMemory::Owned(_) => self.image.write(offset as usize, value),
            LibMemory::Shared { rw_delta, .. } => {
                let ptr = (self.image.base().add(offset as usize) as i64 + rw_delta) as *mut T;
                ptr.write_unaligned(value);
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
fn gnu_dlsym(lib: &LoadedLib, name: &str) -> Option<UserAddr> {
    let table = lib.gnu_hash.as_ref()?;
    let h = gnu_hash(name);

    let nbuckets = unsafe { table.read::<u32>(0) };
    let symoffset = unsafe { table.read::<u32>(4) };
    let bloom_size = unsafe { table.read::<u32>(8) };
    let bloom_shift = unsafe { table.read::<u32>(12) };

    let bloom_off = 16;
    let bloom_idx = ((h as u64 / 64) % bloom_size as u64) as usize;
    let bloom_word = unsafe { table.read::<u64>(bloom_off + bloom_idx * 8) };
    let mask = (1u64 << (h % 64)) | (1u64 << ((h >> bloom_shift) % 64));
    if bloom_word & mask != mask {
        return None;
    }

    let buckets_off = bloom_off + bloom_size as usize * 8;
    let bucket_idx = h % nbuckets;
    let sym_idx = unsafe { table.read::<u32>(buckets_off + bucket_idx as usize * 4) };
    if sym_idx == 0 {
        return None;
    }

    let chains_off = buckets_off + nbuckets as usize * 4;
    let mut i = sym_idx;
    loop {
        let chain_val = unsafe { table.read::<u32>(chains_off + (i - symoffset) as usize * 4) };
        if (chain_val | 1) == (h | 1) {
            let sym = lib.sym(i as usize);
            if sym.st_shndx != 0 && lib.sym_name(i as usize) == name {
                return Some(lib.user_base + sym.st_value);
            }
        }
        if chain_val & 1 != 0 { break; }
        i += 1;
    }
    None
}

/// Read from a file backing directly into a destination pointer.
/// No heap allocation — reads 4KB at a time from the backing.
fn read_backing_into(backing: &dyn crate::file_backing::FileBacking, offset: u64, dst: *mut u8, len: usize) {
    let mut remaining = len;
    let mut file_off = offset;
    let mut buf_off = 0usize;
    let mut page_buf = [0u8; 4096];
    while remaining > 0 {
        let off_in_block = (file_off % 4096) as usize;
        let chunk = (4096 - off_in_block).min(remaining);
        backing.read_page(file_off - off_in_block as u64, &mut page_buf);
        unsafe {
            core::ptr::copy_nonoverlapping(
                page_buf[off_in_block..off_in_block + chunk].as_ptr(),
                dst.add(buf_off),
                chunk,
            );
        }
        file_off += chunk as u64;
        buf_off += chunk;
        remaining -= chunk;
    }
}

/// Load a shared library (.so) into memory for dynamic linking.
/// Reads ELF headers and segments from the file backing (no full-file buffer).
/// Applies RELATIVE relocations and parses PT_DYNAMIC for symbol tables.
/// Returns (LoadedLib, rw_vaddr, rw_end_vaddr) for RW region tracking.
pub fn load_shared_lib(backing: &dyn crate::file_backing::FileBacking) -> Result<(LoadedLib, u64, u64), &'static str> {
    // Read ELF headers (first 4KB covers ELF header + program headers)
    let header_size = 4096.min(backing.file_size() as usize);
    let header_data = crate::process::read_file_range(backing, 0, header_size);
    let layout = parse_layout(&header_data)?;

    let (vaddr_min, vaddr_max) = (layout.vaddr_min, layout.vaddr_max);
    let mut rw_start: Option<u64> = None;
    let mut rw_end: Option<u64> = None;
    for seg in &layout.segments {
        if seg.writable {
            let lo = seg.vaddr;
            let hi = seg.vaddr + seg.memsz;
            rw_start = Some(rw_start.map_or(lo, |v: u64| v.min(lo)));
            rw_end = Some(rw_end.map_or(hi, |v: u64| v.max(hi)));
        }
    }
    let rw_vaddr = rw_start.unwrap_or(vaddr_max);
    let rw_end_vaddr = rw_end.unwrap_or(vaddr_max);

    let load_size = align_2m((vaddr_max - vaddr_min) as usize);
    let t0 = crate::clock::nanos_since_boot();
    let alloc = PageAlloc::new(load_size).ok_or("dlopen: allocation failed")?;
    let t1 = crate::clock::nanos_since_boot();
    let base_ptr = alloc.ptr();
    let image = unsafe { KernelSlice::from_raw(base_ptr, load_size) };

    unsafe { image.zero(); }
    let t2 = crate::clock::nanos_since_boot();

    // Read PT_LOAD segments from backing directly into image
    for seg in &layout.segments {
        let dst = image.ptr_at((seg.vaddr - vaddr_min) as usize);
        read_backing_into(backing, seg.file_offset, dst, seg.filesz as usize);
    }
    let t3 = crate::clock::nanos_since_boot();

    // Parse PT_DYNAMIC from loaded image
    let mut symtab_vaddr = 0u64;
    let mut strtab_vaddr = 0u64;
    let mut strtab_size = 0u64;
    let mut rela_vaddr = 0u64;
    let mut rela_size = 0u64;
    let mut jmprel_vaddr = 0u64;
    let mut jmprel_size = 0u64;
    let mut gnu_hash_vaddr = 0u64;
    let mut init_array_vaddr = 0u64;
    let mut init_array_size = 0u64;
    if let Some((_, dyn_vaddr, dyn_filesz)) = layout.dynamic {
        let dyn_region = image.subslice((dyn_vaddr - vaddr_min) as usize, dyn_filesz as usize);
        let mut off = 0;
        while off + 16 <= dyn_region.size() {
            let d_tag = unsafe { dyn_region.read::<i64>(off) };
            let d_val = unsafe { dyn_region.read::<u64>(off + 8) };
            match d_tag {
                DT_SYMTAB => symtab_vaddr = d_val,
                DT_STRTAB => strtab_vaddr = d_val,
                DT_STRSZ => strtab_size = d_val,
                7 /* DT_RELA */ => rela_vaddr = d_val,
                8 /* DT_RELASZ */ => rela_size = d_val,
                23 /* DT_JMPREL */ => jmprel_vaddr = d_val,
                2 /* DT_PLTRELSZ */ => jmprel_size = d_val,
                d if d == 0x6ffffef5u64 as i64 /* DT_GNU_HASH */ => gnu_hash_vaddr = d_val,
                25 /* DT_INIT_ARRAY */ => init_array_vaddr = d_val,
                27 /* DT_INIT_ARRAYSZ */ => init_array_size = d_val,
                DT_NULL => break,
                _ => {}
            }
            off += 16;
        }
    }

    let gnu_hash_slice = if gnu_hash_vaddr != 0 {
        let off = (gnu_hash_vaddr - vaddr_min) as usize;
        Some(image.subslice(off, image.size() - off))
    } else { None };

    // Determine symbol count. Prefer section header sh_size (includes all symbols:
    // null + hashed exports + unhashed imports). The .gnu_hash only covers hashed
    // exports, so gnu_hash_sym_count underreports when imports exist.
    let sym_count = {
        let mut count = 0;
        if let Some((shoff, shnum, shentsize)) = layout.section_headers {
            let shdr_data = crate::process::read_file_range(backing, shoff, shnum as usize * shentsize as usize);
            let shent = shentsize as usize;
            for i in 0..shnum as usize {
                let base = i * shent;
                if base + 64 > shdr_data.len() { break; }
                let sh_type = u32::from_le_bytes(shdr_data[base + 4..base + 8].try_into().unwrap());
                if sh_type == SHT_DYNSYM {
                    let sh_size = u64::from_le_bytes(shdr_data[base + 32..base + 40].try_into().unwrap());
                    let sh_entsize = u64::from_le_bytes(shdr_data[base + 56..base + 64].try_into().unwrap());
                    count = (sh_size / sh_entsize.max(SYM_SIZE as u64)) as usize;
                    break;
                }
            }
        }
        if count == 0 {
            if let Some(ref gh) = gnu_hash_slice {
                count = gnu_hash_sym_count(gh);
            }
        }
        count
    };

    let dynsym_off = (symtab_vaddr - vaddr_min) as usize;
    let dynstr_off = (strtab_vaddr - vaddr_min) as usize;
    let dynsym = Some(image.subslice(dynsym_off, sym_count * SYM_SIZE));
    let dynstr = Some(image.subslice(dynstr_off, strtab_size as usize));

    // Apply R_X86_64_RELATIVE relocations
    let base_phys = image.phys();
    let mut reloc_count = 0u64;
    let rela_slice = if rela_size > 0 {
        Some(image.subslice((rela_vaddr - vaddr_min) as usize, rela_size as usize))
    } else { None };
    let jmprel_slice = if jmprel_size > 0 {
        Some(image.subslice((jmprel_vaddr - vaddr_min) as usize, jmprel_size as usize))
    } else { None };

    for table in [&rela_slice, &jmprel_slice] {
        let Some(table) = table else { continue };
        let count = table.size() / RELA_SIZE as usize;
        for i in 0..count {
            let rela = unsafe { table.read::<Elf64_Rela>(i * RELA_SIZE as usize) };
            if rela_type(&rela) == R_X86_64_RELATIVE {
                let value = (base_phys as i64 + rela.r_addend) as u64;
                unsafe { image.write::<u64>((rela.r_offset - vaddr_min) as usize, value); }
                reloc_count += 1;
            }
        }
    }

    let mut tls_template: Option<KernelSlice> = None;
    let mut tls_memsz = 0usize;
    let mut tls_align = 0usize;
    let mut eh_frame_hdr_vaddr = 0u64;
    let mut eh_frame_hdr_size = 0u64;
    let (_, tls_vaddr, tls_filesz, tls_memsz_layout, tls_align_layout) =
        ((), layout.tls_vaddr, layout.tls_filesz, layout.tls_memsz, layout.tls_align);
    if tls_memsz_layout > 0 {
        let off = (tls_vaddr - vaddr_min) as usize;
        tls_template = Some(image.subslice(off, tls_filesz));
        tls_memsz = tls_memsz_layout;
        tls_align = tls_align_layout;
    }
    if let Some((ehdr_vaddr, ehdr_size)) = layout.eh_frame_hdr {
        eh_frame_hdr_vaddr = ehdr_vaddr;
        eh_frame_hdr_size = ehdr_size;
    }

    let t4 = crate::clock::nanos_since_boot();
    log!("dlopen: base={:#x} {}MB alloc={}ms zero={}ms copy={}ms reloc={}ms ({} relocs, {} syms)",
        base_phys, load_size / (1024*1024),
        (t1 - t0) / 1_000_000, (t2 - t1) / 1_000_000, (t3 - t2) / 1_000_000,
        (t4 - t3) / 1_000_000, reloc_count, sym_count);

    Ok((LoadedLib { memory: LibMemory::Owned(alloc), user_base: UserAddr::new(base_phys), phys_base: base_phys,
        image, dynsym, dynstr, sym_count,
        tls_template, tls_memsz, tls_align,
        rela: rela_slice, jmprel: jmprel_slice, gnu_hash: gnu_hash_slice,
        cached_relocs: None,
        eh_frame_hdr_vaddr, eh_frame_hdr_size,
        init_array_vaddr, init_array_size,
        user_end: base_phys + vaddr_max - vaddr_min,
    }, rw_vaddr, rw_end_vaddr))
}

/// Rebase all R_X86_64_RELATIVE relocation entries by adding `delta` to each value.
/// Called after user_base is assigned (differs from phys_base used during load_shared_lib).
pub fn rebase_relative_relocs(lib: &LoadedLib, delta: i64) {
    for table in [&lib.rela, &lib.jmprel] {
        let Some(table) = table else { continue };
        let count = table.size() / RELA_SIZE as usize;
        for i in 0..count {
            let rela = unsafe { table.read::<Elf64_Rela>(i * RELA_SIZE as usize) };
            if rela_type(&rela) == R_X86_64_RELATIVE {
                let old_value = unsafe { lib.image.read::<u64>(rela.r_offset as usize) };
                let new_value = (old_value as i64 + delta) as u64;
                unsafe { lib.write_at::<u64>(rela.r_offset, new_value); }
            }
        }
    }
}

/// Look up a symbol by name in a loaded shared library.
pub fn dlsym(lib: &LoadedLib, name: &str) -> Option<UserAddr> {
    for i in 1..lib.sym_count {
        let sym = lib.sym(i);
        if sym.st_shndx == 0 { continue; }
        if lib.sym_name(i) == name {
            return Some(lib.user_base + sym.st_value);
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
        let sym_name = lib.sym_name(r_sym as usize);
        let resolved = other_libs.iter().find_map(|other| gnu_dlsym(other, sym_name));
        if let Some(addr) = resolved {
            unsafe { lib.write_at::<u64>(r_offset, addr.raw()); }
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
        for table in [&lib.rela, &lib.jmprel] {
            let Some(table) = table else { continue };
            let count = table.size() / RELA_SIZE as usize;
            for i in 0..count {
                let rela = unsafe { table.read::<Elf64_Rela>(i * RELA_SIZE as usize) };
                if rela_type(&rela) == 6 || rela_type(&rela) == 7 {
                    resolve_one(rela.r_offset, rela_sym(&rela), &mut resolved_count, &mut unresolved_count);
                }
            }
        }
    }
    log!("dlopen: resolved {} relocs, {} unresolved", resolved_count, unresolved_count);
}

/// Public wrapper for resolve_lib_bind_relocs.
pub fn resolve_lib_bind_relocs_pub(
    lib: &LoadedLib,
    exe_sym_map: &hashbrown::HashMap<&str, UserAddr>,
    libs: &[LoadedLib],
) {
    resolve_lib_bind_relocs(lib, exe_sym_map, libs);
}

/// Adjust all R_X86_64_RELATIVE relocations by delta.
/// Called when a library's user_base differs from the base used during initial relocation.
/// For Owned libs, writes directly to the image. For Shared libs, writes to the private
/// RW copy (RELATIVE relocs always target the RW data segment, never RO text).
pub fn fixup_relative_relocs(lib: &LoadedLib, delta: i64) {
    let (rw_base, rw_offset) = match &lib.memory {
        LibMemory::Shared { rw_alloc, rw_offset, .. } => {
            (Some(rw_alloc.ptr()), *rw_offset)
        }
        _ => (None, 0),
    };

    for table in [&lib.rela, &lib.jmprel] {
        let Some(table) = table else { continue };
        let n = table.size() / RELA_SIZE as usize;
        for i in 0..n {
            let rela = unsafe { table.read::<Elf64_Rela>(i * RELA_SIZE as usize) };
            if rela_type(&rela) != R_X86_64_RELATIVE { continue; }

            let offset = rela.r_offset as usize;
            if let Some(rw_ptr) = rw_base {
                // Shared: write to private RW copy
                let rw_off = offset - rw_offset;
                let ptr = unsafe { rw_ptr.add(rw_off) as *mut u64 };
                let old = unsafe { core::ptr::read_unaligned(ptr) };
                unsafe { core::ptr::write_unaligned(ptr, (old as i64 + delta) as u64); }
            } else {
                // Owned: write to image directly
                let old = unsafe { lib.image.read::<u64>(offset) };
                unsafe { lib.image.write::<u64>(offset, (old as i64 + delta) as u64); }
            }
        }
    }
}

/// Public wrapper for gnu_dlsym.
pub fn gnu_dlsym_pub(lib: &LoadedLib, name: &str) -> Option<UserAddr> {
    gnu_dlsym(lib, name)
}

/// Resolve GLOB_DAT/JUMP_SLOT relocations for a single library.
/// Uses pre-scanned reloc data when available (cached libs) to avoid iterating all entries.
fn resolve_lib_bind_relocs(
    lib: &LoadedLib,
    exe_sym_map: &hashbrown::HashMap<&str, UserAddr>,
    libs: &[LoadedLib],
) {
    let resolve_bind = |r_offset: u64, r_sym: u32| {
        let sym_name = lib.sym_name(r_sym as usize);
        let resolved = exe_sym_map.get(sym_name).copied()
            .or_else(|| libs.iter().find_map(|other| gnu_dlsym(other, sym_name)));
        if let Some(addr) = resolved {
            unsafe { lib.write_at::<u64>(r_offset, addr.raw()); }
        } else {
            log!("dynamic: lib unresolved symbol: {}", sym_name);
        }
    };

    if let Some(relocs) = &lib.cached_relocs {
        for &(r_offset, r_sym) in &relocs.bind {
            resolve_bind(r_offset, r_sym);
        }
    } else {
        for table in [&lib.rela, &lib.jmprel] {
            let Some(table) = table else { continue };
            let count = table.size() / RELA_SIZE as usize;
            for i in 0..count {
                let rela = unsafe { table.read::<Elf64_Rela>(i * RELA_SIZE as usize) };
                match rela_type(&rela) {
                    6 | 7 => resolve_bind(rela.r_offset, rela_sym(&rela)),
                    8 => {
                        unsafe { lib.write_at::<u64>(rela.r_offset, (lib.user_base.raw() as i64 + rela.r_addend) as u64); }
                    }
                    _ => {}
                }
            }
        }
    }
}

/// A single TLS module's layout within the combined TLS block.
#[derive(Clone)]
pub struct TlsModule {
    /// TLS template data (initial values).
    pub template: Option<KernelSlice>,
    /// Total TLS size including BSS (zeroed beyond filesz).
    pub memsz: usize,
    /// Byte offset of this module within the combined TLS block (static modules only).
    pub base_offset: usize,
    /// DTV module ID (1-based). Used by __tls_get_addr to index the DTV.
    pub module_id: u64,
    /// True for modules loaded at process startup (in the static TLS block).
    /// False for dlopen'd modules (TLS allocated on demand via SYS_TLS_ALLOC_BLOCK).
    pub is_static: bool,
}

/// TLS layout info for cross-library TPOFF resolution.
pub struct TlsModuleInfo<'a> {
    pub libs: &'a [LoadedLib],
    pub modules: &'a [TlsModule],
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
            unsafe { lib.write_at::<u64>(r_offset, tpoff as u64); }
        }
        for &(r_offset, r_sym, r_addend) in &relocs.tpoff32 {
            let tpoff = compute_tpoff(lib, r_sym, r_addend, lib_base_offset, total_memsz, tls_info);
            unsafe { lib.write_at::<i32>(r_offset, tpoff as i32); }
        }
        if !relocs.tpoff64.is_empty() || !relocs.tpoff32.is_empty() {
            log!("dlopen: applied {} TPOFF64 + {} TPOFF32 relocs (base_offset={}, total_memsz={})",
                relocs.tpoff64.len(), relocs.tpoff32.len(), lib_base_offset, total_memsz);
        }
    } else {
        // Slow path: scan all relocation entries
        let mut count64 = 0u64;
        let mut count32 = 0u64;
        for table in [&lib.rela, &lib.jmprel] {
            let Some(table) = table else { continue };
            let count = table.size() / RELA_SIZE as usize;
            for i in 0..count {
                let rela = unsafe { table.read::<Elf64_Rela>(i * RELA_SIZE as usize) };
                if rela_type(&rela) == R_X86_64_TPOFF64 {
                    let tpoff = compute_tpoff(lib, rela_sym(&rela), rela.r_addend, lib_base_offset, total_memsz, tls_info);
                    unsafe { lib.write_at::<u64>(rela.r_offset, tpoff as u64); }
                    count64 += 1;
                } else if rela_type(&rela) == R_X86_64_TPOFF32 {
                    let tpoff = compute_tpoff(lib, rela_sym(&rela), rela.r_addend, lib_base_offset, total_memsz, tls_info);
                    unsafe { lib.write_at::<i32>(rela.r_offset, tpoff as i32); }
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

/// Apply R_X86_64_DTPMOD64 and R_X86_64_DTPOFF64 relocations in a shared library.
/// `module_id` is the DTV module index assigned at dlopen time (for same-module TLS).
/// For cross-module TLS (r_sym != 0, symbol is undefined in this lib), the defining
/// module's ID and TLS offset are resolved via `tls_info`.
pub fn apply_dtpmod_relocs(lib: &LoadedLib, module_id: u64, tls_info: &TlsModuleInfo) {
    if let Some(relocs) = &lib.cached_relocs {
        for &(r_offset, r_sym, _r_addend) in &relocs.dtpmod64 {
            let mid = resolve_dtpmod(lib, r_sym, module_id, tls_info);
            unsafe { lib.write_at::<u64>(r_offset, mid); }
        }
        for &(r_offset, r_sym, r_addend) in &relocs.dtpoff64 {
            let offset = resolve_dtpoff(lib, r_sym, r_addend, tls_info);
            unsafe { lib.write_at::<u64>(r_offset, offset as u64); }
        }
        if !relocs.dtpmod64.is_empty() || !relocs.dtpoff64.is_empty() {
            log!("dlopen: applied {} DTPMOD64 + {} DTPOFF64 relocs (module_id={})",
                relocs.dtpmod64.len(), relocs.dtpoff64.len(), module_id);
        }
    } else {
        let mut count_mod = 0u64;
        let mut count_off = 0u64;
        for table in [&lib.rela, &lib.jmprel] {
            let Some(table) = table else { continue };
            let count = table.size() / RELA_SIZE as usize;
            for i in 0..count {
                let rela = unsafe { table.read::<Elf64_Rela>(i * RELA_SIZE as usize) };
                if rela_type(&rela) == R_X86_64_DTPMOD64 {
                    let mid = resolve_dtpmod(lib, rela_sym(&rela), module_id, tls_info);
                    unsafe { lib.write_at::<u64>(rela.r_offset, mid); }
                    count_mod += 1;
                } else if rela_type(&rela) == R_X86_64_DTPOFF64 {
                    let offset = resolve_dtpoff(lib, rela_sym(&rela), rela.r_addend, tls_info);
                    unsafe { lib.write_at::<u64>(rela.r_offset, offset as u64); }
                    count_off += 1;
                }
            }
        }
        if count_mod > 0 || count_off > 0 {
            log!("dlopen: applied {} DTPMOD64 + {} DTPOFF64 relocs (module_id={})",
                count_mod, count_off, module_id);
        }
    }
}

/// Resolve which module ID to write for a DTPMOD64 relocation.
/// If r_sym == 0 (unnamed, same-module LD), use the loading module's ID.
/// If r_sym != 0 but the symbol is defined in this lib (st_shndx != 0), use the loading module's ID.
/// Otherwise, look up the symbol across all loaded libs to find the defining module.
fn resolve_dtpmod(lib: &LoadedLib, r_sym: u32, self_module_id: u64, tls_info: &TlsModuleInfo) -> u64 {
    if r_sym == 0 {
        return self_module_id;
    }
    let sym = lib.sym(r_sym as usize);
    if sym.st_shndx != 0 {
        // Defined in this library
        return self_module_id;
    }
    // Cross-module: find the defining library, then look up its module_id
    let sym_name = lib.sym_name(r_sym as usize);
    for other_lib in tls_info.libs {
        if other_lib.tls_memsz == 0 { continue; }
        if tls_dlsym(other_lib, sym_name).is_some() {
            // Find the TLS module for this library (matched by template pointer)
            let module = tls_info.modules.iter()
                .find(|m| m.template == other_lib.tls_template)
                .unwrap_or_else(|| panic!("dtpmod: no TLS module for lib defining {}", sym_name));
            return module.module_id;
        }
    }
    panic!("dtpmod: unresolved TLS symbol: {}", sym_name);
}

/// Resolve the TLS offset for a DTPOFF64 relocation.
/// If r_sym == 0 (unnamed, LD base), use r_addend (typically 0).
/// If r_sym != 0 and defined in this lib, use st_value + r_addend.
/// Otherwise, look up the symbol across all loaded libs.
fn resolve_dtpoff(lib: &LoadedLib, r_sym: u32, r_addend: i64, tls_info: &TlsModuleInfo) -> i64 {
    if r_sym == 0 {
        return r_addend;
    }
    let sym = lib.sym(r_sym as usize);
    if sym.st_shndx != 0 {
        // Defined in this library — st_value is the offset within its TLS segment
        return sym.st_value as i64 + r_addend;
    }
    // Cross-module: find the defining library and the symbol's TLS offset there
    let sym_name = lib.sym_name(r_sym as usize);
    for other_lib in tls_info.libs {
        if other_lib.tls_memsz == 0 { continue; }
        if let Some(sym_tls_offset) = tls_dlsym(other_lib, sym_name) {
            return sym_tls_offset as i64 + r_addend;
        }
    }
    panic!("dtpoff: unresolved TLS symbol: {}", sym_name);
}

/// Compute a TPOFF value for a relocation entry.
fn compute_tpoff(lib: &LoadedLib, r_sym: u32, r_addend: i64, lib_base_offset: usize, total_memsz: usize, tls_info: &TlsModuleInfo) -> i64 {
    if r_sym != 0 {
        let sym = lib.sym(r_sym as usize);
        if sym.st_shndx != 0 {
            lib_base_offset as i64 + sym.st_value as i64 + r_addend - total_memsz as i64
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
    for idx in 0..lib.sym_count {
        let sym = lib.sym(idx);
        // STT_TLS = 6
        if sym.st_shndx != 0 && (sym.st_info & 0xf) == 6 {
            if lib.sym_name(idx) == name {
                return Some(sym.st_value);
            }
        }
    }
    None
}

/// Resolve a cross-library TPOFF64 relocation by looking up the symbol name
/// from `lib`'s dynsym, finding which library defines it, and computing the tpoff.
fn resolve_cross_lib_tpoff(lib: &LoadedLib, r_sym: u32, tls_info: &TlsModuleInfo, total_memsz: usize) -> i64 {
    let sym_name = lib.sym_name(r_sym as usize);

    // Search all libraries for the defining TLS symbol
    for other_lib in tls_info.libs {
        if other_lib.tls_memsz == 0 { continue; }
        if let Some(sym_tls_offset) = tls_dlsym(other_lib, sym_name) {
            // Find this library's base offset in the combined layout
            let other_base_offset = tls_info.modules.iter()
                .find(|m| m.template == other_lib.tls_template)
                .map(|m| m.base_offset)
                .unwrap_or(0);
            return other_base_offset as i64 + sym_tls_offset as i64 - total_memsz as i64;
        }
    }
    log!("tpoff: unresolved TLS symbol: {}", sym_name);
    0
}
