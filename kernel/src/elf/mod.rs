//! Loading a shared object: the effects half of ELF.
//!
//! Decoding lives in `toyos-elf`, which is pure and host-tested. What is here
//! is everything that touches memory — the image allocation, the segment read,
//! the private writable window, the symbol views over kernel pages — plus the
//! kernel's own ceilings, which are policy and not a property of the format.
//!
//! The rule that governs the whole module: an ELF is untrusted input, so
//! nothing here panics on a malformed one. Refusals are `&'static str` because
//! every caller logs them beside the path.

// Every unsafe block under `elf::` carries a `SAFETY:` comment — measured
// and documented in full by `issues/build/clippy-has-never-run-here.md`'s
// per-area plan. `host-tests.yml`'s kernel clippy step already runs with
// `-D warnings`, so `warn` here is what actually gates: a new undocumented
// block anywhere in this module tree fails CI, while the rest of the kernel
// (not yet swept) stays silent.
#![warn(clippy::undocumented_unsafe_blocks)]

mod cache;
mod index;
mod reloc;

pub use cache::{cache_loaded_lib, try_clone_cached, CachedRelocs};
pub use index::{parse_rela_entries, ParsedRelaEntries, RelocationIndex};
pub use reloc::{
    apply_dtpmod_relocs, apply_tpoff_relocs, rebase_relative_relocs, resolve_dlopen_relocs,
    resolve_lib_bind_relocs,
};

use crate::mm::{align_2m_checked, KernelSlice, MAX_HEAP_ALLOC, PAGE_2M};
use crate::process::PageAlloc;
use crate::UserAddr;
use toyos_elf::dynamic::Dynamic;
use toyos_elf::section::{SectionTable, SHT_DYNSYM};
use toyos_elf::sym::SymTab;
use toyos_elf::{rela, GnuHash, Layout, RelaTable};

/// The crate's TLS alignment ceiling is the largest page the kernel maps, and
/// that is a claim the kernel has to keep true.
const _: () = assert!(toyos_elf::MAX_TLS_ALIGN == PAGE_2M);

/// [`Layout::parse`] plus the one ceiling that is the kernel's and not the
/// format's.
///
/// Four call sites read the section header table whole into one `Vec`,
/// including one in `process.rs` with no failure path. Refuse the file here
/// rather than let each of them meet the heap's assert separately.
pub fn parse_layout(data: &[u8]) -> Result<Layout, &'static str> {
    let layout = Layout::parse(data).map_err(|e| e.as_str())?;
    if layout
        .section_headers
        .is_some_and(|s| s.byte_len() > MAX_HEAP_ALLOC)
    {
        return Err("ELF: section header table larger than one kernel allocation");
    }
    Ok(layout)
}

/// Ownership model for a loaded shared library's memory.
pub enum LibMemory {
    /// Fresh load: one allocation owns everything.
    Owned(PageAlloc),
    /// Cloned from cache: read-only pages are shared (owned by the cache),
    /// writable pages are private.
    Shared {
        rw_alloc: PageAlloc,
        cached_image: KernelSlice,
        /// 2 MiB-aligned offset within the cached image where the private
        /// writable region starts.
        rw_offset: usize,
        /// Added to a cached address to reach the private writable copy.
        rw_delta: i64,
    },
}

/// One TLS module's placement within a thread's combined TLS block.
#[derive(Clone)]
pub struct TlsModule {
    pub template: Option<KernelSlice>,
    /// Total TLS size including `.tbss`, which is zeroed rather than copied.
    pub memsz: usize,
    /// Byte offset of this module within the combined block (static modules
    /// only).
    pub base_offset: usize,
    /// DTV module ID, 1-based. `__tls_get_addr` indexes the DTV with it.
    pub module_id: u64,
    /// True for modules present at process startup. A `dlopen`ed module's
    /// block is allocated on demand through `SYS_TLS_ALLOC_BLOCK`.
    pub is_static: bool,
}

/// Everything a cross-module TLS relocation has to resolve against.
pub struct TlsModuleInfo<'a> {
    pub libs: &'a [LoadedLib],
    pub modules: &'a [TlsModule],
}

pub struct LoadedLib {
    pub memory: LibMemory,
    pub user_base: UserAddr,
    /// Physical base address, for page table mappings.
    pub phys_base: u64,
    /// Bounds-checked view of the whole loaded image.
    pub image: KernelSlice,
    /// `.dynsym`, clamped to the entries it actually holds.
    dynsym: Option<KernelSlice>,
    dynstr: Option<KernelSlice>,
    pub tls_template: Option<KernelSlice>,
    pub tls_memsz: usize,
    pub tls_align: usize,
    rela: Option<KernelSlice>,
    jmprel: Option<KernelSlice>,
    gnu_hash: Option<KernelSlice>,
    cached_relocs: Option<CachedRelocs>,
    /// `.eh_frame_hdr`, relative to the module base, from `PT_GNU_EH_FRAME`.
    pub eh_frame_hdr_vaddr: u64,
    pub eh_frame_hdr_size: u64,
    /// `.init_array`, relative to the module base, from `DT_INIT_ARRAY`.
    pub init_array_vaddr: u64,
    pub init_array_size: u64,
    /// Bytes between the image's lowest and highest virtual address.
    pub span: u64,
    /// The image-relative half-open range every writable `PT_LOAD` of this
    /// module falls in, and `(span, span)` for a module with none.
    ///
    /// **What a page of a mapped library gets its protection from.** Exact,
    /// unlike the 2 MiB-rounded `rw_offset`/`rw_size` beside it: those size the
    /// private copy, where rounding outwards costs a page of memory — rounding
    /// a *protection* outwards costs a writable page of somebody's `.text`.
    pub rw_lo: u64,
    pub rw_hi: u64,
}

impl LoadedLib {
    /// What the 4 KiB page `offset` bytes into this image may be used for.
    ///
    /// Three zones, from the module's own program headers: code below the
    /// writable window, data inside it, constants above it. A read-only
    /// non-executable segment placed *below* `.text` by some other linker would
    /// come out executable here — over-permissive, never writable, and the
    /// exact segment map is not carried past `load_shared_lib`.
    fn page_prot(&self, offset: u64) -> crate::mm::paging::Prot {
        use crate::mm::paging::Prot;
        if offset < self.rw_lo {
            Prot::ReadExec
        } else if offset < self.rw_hi {
            Prot::ReadWrite
        } else {
            Prot::Read
        }
    }

    /// Give this module a virtual address in `pt` and map its pages there.
    ///
    /// **One pass, one window at a time, and no window is mapped twice.** The
    /// shape this replaces mapped the whole image writable and then re-mapped
    /// the private window over the top of it, which meant every library's
    /// `.text` was writable in every process that loaded it — and, once the
    /// image was cached, writable in *every* process at the same physical
    /// pages.
    ///
    /// A `Shared` module's private copy starts at a 2 MiB boundary rounded down
    /// from `rw_lo`, so the window it starts in holds the tail of `.text` as
    /// well. That window is the split one: its code pages come out `ReadExec`
    /// over the private copy, which holds a byte-identical copy of them.
    pub fn map_into(&self, pt: &crate::process::PageTables) -> Option<UserAddr> {
        use crate::mm::paging::WindowProt;

        let (image_phys, image_size) = match &self.memory {
            LibMemory::Owned(alloc) => (
                crate::DirectMap::phys_of(alloc.ptr()),
                alloc.size() as u64,
            ),
            LibMemory::Shared { cached_image, .. } => {
                (cached_image.phys(), cached_image.size() as u64)
            }
        };

        let base = pt
            .lock()
            .alloc_region(image_size, crate::vma::RegionKind::Mapped)?;

        let mut offset = 0;
        while offset < image_size {
            // Which physical frame backs this window: the cache's shared image,
            // except where this process has a private writable copy.
            let phys = match &self.memory {
                LibMemory::Owned(_) => image_phys + offset,
                LibMemory::Shared { rw_alloc, rw_offset, .. } => {
                    let lo = *rw_offset as u64;
                    if (lo..lo + rw_alloc.size() as u64).contains(&offset) {
                        crate::DirectMap::phys_of(rw_alloc.ptr()) + (offset - lo)
                    } else {
                        image_phys + offset
                    }
                }
            };
            let mut prot = WindowProt::uniform(crate::mm::paging::Prot::Read);
            let mut page = 0;
            while page < PAGE_2M {
                prot.set(page, self.page_prot(offset + page));
                page += 4096;
            }
            pt.lock().map_window(UserAddr::new(base.raw() + offset), phys, &prot);
            offset += PAGE_2M;
        }
        Some(base)
    }

    /// This module's symbol table, or an empty one when it declares none.
    ///
    /// The slices are kernel pages this `LoadedLib` either owns or shares with
    /// the cache, so they live exactly as long as the borrow. The shape this
    /// replaces was an `expect("no dynsym")` that four separate callers each
    /// happened to guard with a different check.
    pub fn symbols(&self) -> SymTab<'_> {
        let Some(syms) = &self.dynsym else {
            return SymTab::empty();
        };
        // A module with `DT_SYMTAB` and no `DT_STRTAB` still has that many
        // symbols; it just cannot name any of them, and an unnamed symbol
        // matches nothing. Losing the *count* would instead make every
        // relocation naming one look out of range.
        let strs: &[u8] = match &self.dynstr {
            // SAFETY: `dynstr` is `Some` only when `load_shared_lib` built it
            // through `ModuleImage::slice`, which bounds-checked the file's
            // declared vaddr/size against `vaddr_min..vaddr_max` before ever
            // returning a `KernelSlice` — see `KernelSlice::as_slice`'s
            // `# Safety`. `image` (and everything subsliced from it) lives as
            // long as this `LoadedLib` does, which outlives this borrow.
            Some(strs) => unsafe { strs.as_slice() },
            None => &[],
        };
        // SAFETY: same bounds argument as `dynstr` above, for `syms`
        // (`self.dynsym`) — `load_shared_lib` additionally clamps its size to
        // the declared symbol count (see its own comment there), so `syms`
        // covers exactly the entries `SymTab` is allowed to index.
        unsafe { SymTab::new(syms.as_slice(), strs) }
    }

    pub fn sym_count(&self) -> usize {
        self.symbols().count()
    }

    fn gnu_hash(&self) -> Option<GnuHash<'_>> {
        // SAFETY: same bounds argument as `symbols` above — `self.gnu_hash`
        // is `Some` only through `ModuleImage::slice_to_end`, which is
        // `slice` (bounds-checked) under the hood.
        GnuHash::parse(unsafe { self.gnu_hash.as_ref()?.as_slice() })
    }

    /// Every relocation in this module's `DT_RELA` and `DT_JMPREL` tables.
    fn relocations(&self) -> impl Iterator<Item = toyos_elf::Rela> + '_ {
        table_entries(&self.rela).chain(table_entries(&self.jmprel))
    }

    /// One past the last virtual address this module occupies.
    ///
    /// Derived rather than stored: it was a field, and four sites added the
    /// same delta to it that they had just added to `user_base`.
    pub fn user_end(&self) -> u64 {
        self.user_base.raw() + self.span
    }

    /// The address this module gives a symbol it defines.
    pub fn resolve(&self, name: &str) -> Option<UserAddr> {
        let symbols = self.symbols();
        let idx = self.gnu_hash()?.lookup(name, &symbols)?;
        Some(self.user_base + symbols.get(idx)?.value)
    }

    /// A `STT_TLS` symbol's offset within this module's TLS segment.
    pub fn resolve_tls(&self, name: &str) -> Option<u64> {
        self.symbols().find_tls(name)
    }
}

/// Look up a symbol by name, walking `.dynsym` rather than the hash table.
///
/// `SYS_DLSYM`'s path: a caller that asks by name once has no use for a hash
/// table walk, and a module may carry symbols `.gnu.hash` does not index.
pub fn dlsym(lib: &LoadedLib, name: &str) -> Option<UserAddr> {
    let symbols = lib.symbols();
    symbols.find(name).map(|(_, sym)| lib.user_base + sym.value)
}

/// A loaded module image, addressed by the module's own virtual addresses.
///
/// Every `DT_*` tag and every `r_offset` in a shared object is a file-supplied
/// vaddr the loader has to turn into an offset into this image. Written inline
/// as `vaddr - vaddr_min` that is a wrapping subtraction on untrusted input,
/// producing an offset whose `offset + size` wraps back under the slice's own
/// bounds check. Every such conversion goes through here, and it returns an
/// error rather than asserting: a malformed `.so` is untrusted input, not a
/// kernel bug.
struct ModuleImage {
    image: KernelSlice,
    vaddr_min: u64,
    vaddr_max: u64,
}

impl ModuleImage {
    fn slice(&self, vaddr: u64, size: u64) -> Result<KernelSlice, &'static str> {
        let end = vaddr.checked_add(size).ok_or("ELF: dynamic extent overflows")?;
        if vaddr < self.vaddr_min || end > self.vaddr_max {
            return Err("ELF: dynamic table outside the loaded image");
        }
        Ok(self
            .image
            .subslice((vaddr - self.vaddr_min) as usize, size as usize))
    }

    /// From `vaddr` to the end of the image, for a table whose size no tag
    /// records — `.gnu.hash` is walked structurally, not by a declared length.
    fn slice_to_end(&self, vaddr: u64) -> Result<KernelSlice, &'static str> {
        self.slice(vaddr, self.vaddr_max.saturating_sub(vaddr))
    }

    fn optional(&self, vaddr: Option<u64>, size: u64) -> Result<Option<KernelSlice>, &'static str> {
        match vaddr {
            Some(v) => Ok(Some(self.slice(v, size)?)),
            None => Ok(None),
        }
    }
}

/// Read `dst.size()` bytes from a file backing straight into `dst`, without a
/// heap buffer in between.
///
/// **The destination is a window and not a `(*mut u8, usize)` pair**, and that
/// is the whole of what bounds this function: `dst` came from an allocation
/// that sized it, so the length read is the length allocated and neither this
/// loop nor any caller can name a third number. The pair this replaced made the
/// "valid for `len` bytes" requirement real at three call sites and enforced it
/// at none of them.
///
/// `Err` on the first page the store would not give up, with the destination
/// holding zeros from there on. Every caller refuses rather than continues: an
/// image assembled from a failed read is a process built out of a hole, and the
/// fault it eventually takes says nothing about the disk that caused it.
#[must_use = "an image assembled from a failed read is zeros, not the program"]
pub(crate) fn read_backing_into(
    backing: &dyn crate::file_backing::FileBacking,
    offset: u64,
    dst: KernelSlice,
) -> crate::block::BlockResult {
    let mut remaining = dst.size();
    let mut file_off = offset;
    let mut buf_off = 0usize;
    let mut page_buf = [0u8; 4096];
    while remaining > 0 {
        let off_in_block = (file_off % 4096) as usize;
        let chunk = (4096 - off_in_block).min(remaining);
        backing.read_page(file_off - off_in_block as u64, &mut page_buf)?;
        // SAFETY: `copy_from` asserts `buf_off + chunk <= dst.size()` against
        // the allocation `dst` was built from, so the write lands inside it —
        // the loop's own arithmetic (`remaining` shrinks by `chunk` and never
        // past 0) is now a second argument rather than the only one. `page_buf`
        // is this frame's stack array and `dst` is heap or PMM pages, so the
        // two cannot overlap, and nothing else can see `dst` while a caller is
        // still filling it.
        unsafe { dst.copy_from(buf_off, &page_buf[off_in_block..off_in_block + chunk]) };
        file_off += chunk as u64;
        buf_off += chunk;
        remaining -= chunk;
    }
    Ok(())
}

/// Load a shared object into one contiguous allocation and apply its
/// `R_X86_64_RELATIVE` relocations.
///
/// Returns the module plus the 2 MiB-aligned writable window inside it, which
/// the caller needs to cache it: the window a relocation was validated against
/// and the window `rw_alloc` later covers must be the same one.
pub fn load_shared_lib(
    backing: &dyn crate::file_backing::FileBacking,
) -> Result<(LoadedLib, usize, usize), &'static str> {
    let header_size = 4096.min(backing.file_size() as usize);
    let header_data = crate::process::read_file_range(backing, 0, header_size);
    let layout = parse_layout(&header_data)?;

    let (vaddr_min, vaddr_max) = (layout.vaddr_min, layout.vaddr_max);
    // No writable segment leaves an empty window at the top of the image, which
    // is where a relocation may then not write at all.
    let (rw_lo, rw_hi) = layout.writable_window().unwrap_or((layout.span(), layout.span()));

    // The span is a number the file chose, so the round-up wraps — and a
    // wrapped `load_size` is an allocation smaller than every offset later
    // computed against it.
    let (Some(load_size), Some(rw_end_aligned)) = (
        usize::try_from(layout.span()).ok().and_then(align_2m_checked),
        usize::try_from(rw_hi).ok().and_then(align_2m_checked),
    ) else {
        return Err("ELF: image span does not fit an allocation");
    };
    let rw_offset = rw_lo as usize & !(PAGE_2M as usize - 1);
    let rw_size = rw_end_aligned - rw_offset;

    let t0 = crate::clock::nanos_since_boot();
    let alloc =
        PageAlloc::new(load_size, crate::mm::pmm::Category::Elf).ok_or("dlopen: allocation failed")?;
    let t1 = crate::clock::nanos_since_boot();
    // The window is the allocation's own. `load_size` sized the request and is
    // not repeated here: what every offset in this function is bounded against
    // is what the PMM actually handed over, so a `load_size` that drifted from
    // the allocation cannot become a write past it. Every past out-of-bounds in
    // this loader was that drift.
    let image = alloc.window();

    // SAFETY: `image` was just built from the fresh, exclusively-owned
    // `alloc` above — nothing else has a reference to it yet, so `zero`'s
    // exclusivity requirement (see `KernelSlice::write`'s `# Safety`, which
    // `zero` shares) holds trivially.
    unsafe {
        image.zero();
    }
    let t2 = crate::clock::nanos_since_boot();

    // `image` is sized from every segment's `p_memsz`, so reading `p_filesz`
    // bytes into it stays in bounds only because `Layout` guarantees
    // `filesz <= memsz`. Take the checked subslice rather than a bare pointer,
    // so a weakening of that invariant is an assert and not an overwrite of
    // whatever the PMM handed out after this allocation.
    for seg in layout.segments() {
        let dst = image.subslice((seg.vaddr - vaddr_min) as usize, seg.filesz as usize);
        read_backing_into(backing, seg.file_offset, dst)
            .map_err(|_| "a segment could not be read off the device")?;
    }
    let t3 = crate::clock::nanos_since_boot();

    let module = ModuleImage { image, vaddr_min, vaddr_max };
    let dyn_info = match layout.dynamic {
        Some((_, vaddr, size)) => {
            let region = module.slice(vaddr, size)?;
            // SAFETY: `region` came from `ModuleImage::slice`, which
            // bounds-checked `vaddr`/`size` against `vaddr_min..vaddr_max`
            // before returning — see `KernelSlice::as_slice`'s `# Safety`.
            // `image` is exclusively owned here, before `LoadedLib` (and any
            // reader of it) exists.
            Dynamic::parse(unsafe { region.as_slice() })
        }
        None => Dynamic::default(),
    };

    let gnu_hash = match dyn_info.gnu_hash {
        Some(vaddr) => Some(module.slice_to_end(vaddr)?),
        None => None,
    };

    // Prefer the section header's `sh_size`, which counts every symbol — the
    // null entry, the hashed exports and the unhashed imports. `.gnu.hash`
    // indexes only the exports, so a count derived from it is short whenever
    // the module imports anything.
    let declared = dynsym_count_from_sections(backing, &layout)
        .filter(|&n| n > 0)
        .or_else(|| {
            // SAFETY: same bounds argument as `dyn_info` above — `gnu_hash`
            // came from `module.slice_to_end`, which is `slice` under the
            // hood, still within `load_shared_lib`'s exclusive-construction
            // phase.
            let table = GnuHash::parse(unsafe { gnu_hash.as_ref()?.as_slice() })?;
            table.sym_count()
        })
        .unwrap_or(0);

    // `.dynsym` runs to the end of the image and is then clamped to the count,
    // so a relocation's `r_sym` is bounded by what the image can hold *and* by
    // what the table declares, whichever is smaller.
    let dynsym = match dyn_info.symtab {
        Some(vaddr) => {
            let whole = module.slice_to_end(vaddr)?;
            let count = declared.min(whole.size() / toyos_elf::sym::ENTRY_SIZE);
            Some(whole.subslice(0, count * toyos_elf::sym::ENTRY_SIZE))
        }
        None => None,
    };
    let sym_count = dynsym.as_ref().map_or(0, |s| s.size() / toyos_elf::sym::ENTRY_SIZE);
    let dynstr = module.optional(dyn_info.strtab, dyn_info.strsz.unwrap_or(0))?;

    let rela = match dyn_info.rela {
        Some(t) => Some(module.slice(t.vaddr, t.size)?),
        None => None,
    };
    let jmprel = match dyn_info.jmprel {
        Some(t) => Some(module.slice(t.vaddr, t.size)?),
        None => None,
    };

    // Validate every entry the loader will ever write, before it writes the
    // first: a module refused halfway through has already been modified, and a
    // `DTPOFF64` with `r_sym == 0` writes `r_addend` verbatim — so an
    // unvalidated `r_offset` is an arbitrary 8-byte kernel write with a
    // file-chosen value.
    //
    // The bound is the writable window, not the image: once this module is
    // cached the write lands in `rw_alloc`, which covers only that window.
    //
    // The window is image-relative, and so is `LoadedLib::write_at`'s offset —
    // but the `RELATIVE` pass below goes through `ModuleImage::slice`, which
    // subtracts `vaddr_min`. The two agree for every `vaddr_min` of zero, which
    // is every module a linker produces; above zero `slice` refuses rather than
    // writing somewhere else, so the disagreement is a refusal and not a
    // corruption.
    let window = (rw_offset as u64, (rw_offset + rw_size) as u64);
    let entries = table_entries(&rela).chain(table_entries(&jmprel));
    rela::validate(entries, window, sym_count).map_err(|e| e.as_str())?;

    let base_phys = image.phys();
    let mut reloc_count = 0u64;
    for entry in table_entries(&rela).chain(table_entries(&jmprel)) {
        if entry.kind == toyos_elf::RelocKind::Relative {
            let value = (base_phys as i64 + entry.addend) as u64;
            // SAFETY: `write::<u64>` requires 8 valid, unaliased bytes at
            // this offset. `module.slice(entry.offset, 8)?` re-derives that
            // from `ModuleImage::slice`'s own bounds check, independent of
            // the `rela::validate` pass above — an `entry.offset` outside the
            // image is an `Err` here via `?`, not a write. `image` is still
            // exclusively owned within `load_shared_lib`, before any reader
            // (`LoadedLib::symbols`, `dlsym`, …) can see it.
            unsafe { module.slice(entry.offset, 8)?.write::<u64>(0, value) };
            reloc_count += 1;
        }
    }

    let (tls_template, tls_memsz, tls_align) = match layout.tls {
        Some(tls) => (
            Some(module.slice(tls.vaddr, tls.filesz)?),
            tls.memsz as usize,
            tls.align as usize,
        ),
        None => (None, 0, 0),
    };
    let (eh_frame_hdr_vaddr, eh_frame_hdr_size) = layout.eh_frame_hdr.unwrap_or((0, 0));
    let init_array = dyn_info.init_array;

    let t4 = crate::clock::nanos_since_boot();
    log!(
        "dlopen: base={:#x} {}MB alloc={}ms zero={}ms copy={}ms reloc={}ms ({} relocs, {} syms)",
        base_phys,
        load_size / (1024 * 1024),
        (t1 - t0) / 1_000_000,
        (t2 - t1) / 1_000_000,
        (t3 - t2) / 1_000_000,
        (t4 - t3) / 1_000_000,
        reloc_count,
        sym_count
    );

    Ok((
        LoadedLib {
            memory: LibMemory::Owned(alloc),
            user_base: UserAddr::new(base_phys),
            phys_base: base_phys,
            image,
            dynsym,
            dynstr,
            tls_template,
            tls_memsz,
            tls_align,
            rela,
            jmprel,
            gnu_hash,
            cached_relocs: None,
            eh_frame_hdr_vaddr,
            eh_frame_hdr_size,
            init_array_vaddr: init_array.map_or(0, |t| t.vaddr),
            init_array_size: init_array.map_or(0, |t| t.size),
            span: layout.span(),
            rw_lo,
            rw_hi,
        },
        rw_offset,
        rw_size,
    ))
}

/// `.dynsym`'s entry count as the section header table declares it.
///
/// `None` when there is no section header table, no `SHT_DYNSYM` in it, or the
/// read came back short — each of which is "the file did not say", which the
/// caller answers by asking `.gnu.hash` instead.
fn dynsym_count_from_sections(
    backing: &dyn crate::file_backing::FileBacking,
    layout: &Layout,
) -> Option<usize> {
    let table = layout.section_headers?;
    let bytes = crate::process::read_file_range(backing, table.file_offset, table.byte_len());
    let dynsym = SectionTable::new(&bytes).find(SHT_DYNSYM)?;
    let entry_size = dynsym.entry_size.max(toyos_elf::sym::ENTRY_SIZE as u64);
    Some((dynsym.size / entry_size) as usize)
}

/// The entries of one optional relocation table.
///
/// A free function rather than a closure because the iterator borrows the
/// kernel pages the table lives in, not the caller's frame.
fn table_entries(table: &Option<KernelSlice>) -> impl Iterator<Item = toyos_elf::Rela> + '_ {
    // SAFETY: every `KernelSlice` this is ever called with (`LoadedLib.rela`/
    // `.jmprel`, or the local bindings of the same names in
    // `load_shared_lib`) came from `ModuleImage::slice`'s bounds check.
    // Relocation tables are read-only data: nothing in this module writes
    // back into the `rela`/`jmprel` range once built — the `RELATIVE` pass
    // above writes into `image`'s data through `module.slice(entry.offset,
    // 8)`, a different range — so a shared `&[u8]` here never races a writer.
    table
        .iter()
        .flat_map(|slice| RelaTable::new(unsafe { slice.as_slice() }).iter())
}
