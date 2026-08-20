//! The shared-object cache: one image in memory, one private writable window
//! per process.
//!
//! A cached module's read-only pages are mapped into every process that loads
//! it and its base address never moves, so its `R_X86_64_RELATIVE` relocations
//! need no rework. Only the writable window is copied.

use alloc::string::String;
use alloc::vec::Vec;

use super::{LibMemory, LoadedLib};
use crate::mm::{KernelSlice, MAX_HEAP_ALLOC};
use crate::process::PageAlloc;
use crate::sync::Lock;
use crate::UserAddr;
use toyos_elf::{RelaCounts, RelocKind};

/// A module's non-`RELATIVE` relocations, extracted once at cache time.
///
/// A library is about 99.5 % `RELATIVE` and none of those are kept, so this
/// saves iterating 211 K entries on every clone to find the thousand that
/// matter.
#[derive(Clone)]
pub struct CachedRelocs {
    /// `GLOB_DAT` and `JUMP_SLOT`: (offset, symbol).
    pub bind: Vec<(u64, u32)>,
    pub tpoff64: Vec<(u64, u32, i64)>,
    pub tpoff32: Vec<(u64, u32, i64)>,
    /// The kernel writes a module id here.
    pub dtpmod64: Vec<(u64, u32, i64)>,
    /// The kernel writes a TLS offset within the module here.
    pub dtpoff64: Vec<(u64, u32, i64)>,
}

/// Extract every non-`RELATIVE` entry, or `None` when the result would not fit
/// one kernel allocation.
///
/// Counted by kind and reserved exactly, never grown: bounding by the tables'
/// total size would refuse to cache the largest library in the tree, and
/// growth by doubling asks the page source for more than the ceiling it just
/// passed. These tables are `KernelSlice`s over the loaded image and are
/// bounded by the image's own size rather than by `MAX_HEAP_ALLOC`, so a large
/// enough `.so` reaches the heap's assert through here.
///
/// Refusing costs only the cache: every consumer of `cached_relocs` falls back
/// to scanning the tables directly.
fn prescan_relocs(lib: &LoadedLib) -> Option<CachedRelocs> {
    let counts = RelaCounts::of(lib.relocations());
    let widest = core::mem::size_of::<(u64, u32, i64)>();
    // Only the kinds this keeps. `relative` is 99.5 % of a real library and
    // none of them are stored, so bounding on it would refuse to cache
    // every library in the tree.
    let kept = [RelocKind::GlobDat, RelocKind::Tpoff64, RelocKind::Tpoff32,
        RelocKind::DtpMod64, RelocKind::DtpOff64];
    if counts.max_of(&kept).checked_mul(widest).is_none_or(|b| b > MAX_HEAP_ALLOC) {
        log!("dlopen: prescan {:?} will not fit one allocation, not caching", counts);
        return None;
    }
    let mut relocs = CachedRelocs {
        bind: Vec::with_capacity(counts.bind),
        tpoff64: Vec::with_capacity(counts.tpoff64),
        tpoff32: Vec::with_capacity(counts.tpoff32),
        dtpmod64: Vec::with_capacity(counts.dtpmod64),
        dtpoff64: Vec::with_capacity(counts.dtpoff64),
    };
    for r in lib.relocations() {
        match r.kind {
            RelocKind::GlobDat | RelocKind::JumpSlot => relocs.bind.push((r.offset, r.sym)),
            RelocKind::Tpoff64 => relocs.tpoff64.push((r.offset, r.sym, r.addend)),
            RelocKind::Tpoff32 => relocs.tpoff32.push((r.offset, r.sym, r.addend)),
            RelocKind::DtpMod64 => relocs.dtpmod64.push((r.offset, r.sym, r.addend)),
            RelocKind::DtpOff64 => relocs.dtpoff64.push((r.offset, r.sym, r.addend)),
            _ => {}
        }
    }
    Some(relocs)
}

/// Everything about a loaded module that does not depend on where it is
/// mapped.
///
/// The one place these fields are listed twice instead of three times. Both
/// the cache entry and every clone of it are the same bytes at the same
/// physical address; only the memory ownership, the assigned user base and the
/// pre-scanned relocations differ.
#[derive(Clone, Copy)]
struct Snapshot {
    image: KernelSlice,
    dynsym: Option<KernelSlice>,
    dynstr: Option<KernelSlice>,
    tls_template: Option<KernelSlice>,
    tls_memsz: usize,
    tls_align: usize,
    rela: Option<KernelSlice>,
    jmprel: Option<KernelSlice>,
    gnu_hash: Option<KernelSlice>,
    eh_frame_hdr_vaddr: u64,
    eh_frame_hdr_size: u64,
    init_array_vaddr: u64,
    init_array_size: u64,
    span: u64,
    rw_lo: u64,
    rw_hi: u64,
}

impl Snapshot {
    fn of(lib: &LoadedLib) -> Snapshot {
        Snapshot {
            image: lib.image,
            dynsym: lib.dynsym,
            dynstr: lib.dynstr,
            tls_template: lib.tls_template,
            tls_memsz: lib.tls_memsz,
            tls_align: lib.tls_align,
            rela: lib.rela,
            jmprel: lib.jmprel,
            gnu_hash: lib.gnu_hash,
            eh_frame_hdr_vaddr: lib.eh_frame_hdr_vaddr,
            eh_frame_hdr_size: lib.eh_frame_hdr_size,
            init_array_vaddr: lib.init_array_vaddr,
            init_array_size: lib.init_array_size,
            span: lib.span,
            rw_lo: lib.rw_lo,
            rw_hi: lib.rw_hi,
        }
    }

    fn into_lib(
        self,
        memory: LibMemory,
        user_base: UserAddr,
        cached_relocs: Option<CachedRelocs>,
    ) -> LoadedLib {
        LoadedLib {
            memory,
            user_base,
            phys_base: self.image.phys(),
            image: self.image,
            dynsym: self.dynsym,
            dynstr: self.dynstr,
            tls_template: self.tls_template,
            tls_memsz: self.tls_memsz,
            tls_align: self.tls_align,
            rela: self.rela,
            jmprel: self.jmprel,
            gnu_hash: self.gnu_hash,
            cached_relocs,
            eh_frame_hdr_vaddr: self.eh_frame_hdr_vaddr,
            eh_frame_hdr_size: self.eh_frame_hdr_size,
            init_array_vaddr: self.init_array_vaddr,
            init_array_size: self.init_array_size,
            span: self.span,
            rw_lo: self.rw_lo,
            rw_hi: self.rw_hi,
        }
    }
}

/// An immortal image, used as the template every later load clones from.
struct CachedLib {
    alloc: PageAlloc,
    snapshot: Snapshot,
    rw_offset: usize,
    rw_size: usize,
    relocs: CachedRelocs,
}

static SO_CACHE: Lock<Vec<(String, CachedLib)>> = Lock::new(Vec::new());

/// Take ownership of a freshly loaded library and hand back a clone of it in
/// `Shared` mode, with a private writable window.
///
/// `rw_offset`/`rw_size` come from `load_shared_lib` so that the window a
/// relocation was validated against and the window `rw_alloc` covers cannot
/// drift apart. A library that cannot be cached is handed back unchanged: this
/// is an optimisation, and refusing it costs only the next load's time.
pub fn cache_loaded_lib(path: &str, lib: LoadedLib, rw_offset: usize, rw_size: usize) -> LoadedLib {
    if !matches!(lib.memory, LibMemory::Owned(_)) {
        return lib;
    }
    let snapshot = Snapshot::of(&lib);
    let user_base = lib.user_base;
    // Before the allocation moves out of `lib`, because the scan reads the
    // relocation tables through it.
    let scanned = prescan_relocs(&lib);
    let LibMemory::Owned(alloc) = lib.memory else {
        unreachable!("the check above established this")
    };

    // Refusing to prescan means refusing to cache: the cached image is what
    // `cached_relocs` describes, so a lib without one must keep taking the
    // scan-every-table path.
    let owned = |alloc| snapshot.into_lib(LibMemory::Owned(alloc), user_base, None);
    let Some(relocs) = scanned else {
        return owned(alloc);
    };
    log!(
        "dlopen: cached {} with {} bind + {} tpoff64 + {} tpoff32 + {} dtpmod64 + {} dtpoff64 pre-scanned relocs",
        path, relocs.bind.len(), relocs.tpoff64.len(), relocs.tpoff32.len(),
        relocs.dtpmod64.len(), relocs.dtpoff64.len()
    );

    let Some(rw_alloc) = PageAlloc::new(rw_size, crate::mm::pmm::Category::Elf) else {
        return owned(alloc);
    };
    let alloc_ptr = alloc.ptr();
    unsafe {
        core::ptr::copy_nonoverlapping(alloc_ptr.add(rw_offset), rw_alloc.ptr(), rw_size);
    }
    let rw_delta = rw_alloc.ptr() as i64 - (alloc_ptr as i64 + rw_offset as i64);

    SO_CACHE.lock().push((
        String::from(path),
        CachedLib { alloc, snapshot, rw_offset, rw_size, relocs: relocs.clone() },
    ));

    snapshot.into_lib(
        LibMemory::Shared {
            rw_alloc,
            cached_image: snapshot.image,
            rw_offset,
            rw_delta,
        },
        user_base,
        Some(relocs),
    )
}

/// Clone a library out of the cache by path, or `None` when it is not there.
pub fn try_clone_cached(path: &str) -> Option<LoadedLib> {
    let cache = SO_CACHE.lock();
    let idx = cache.iter().position(|(p, _)| p == path)?;
    clone_from_cache(&cache[idx].1)
}

/// Share the read-only pages, copy the writable ones.
///
/// The base address stays the cache's, so `RELATIVE` relocations need no fixup
/// until `spawn` or `dlopen` assigns the module a user address.
fn clone_from_cache(cached: &CachedLib) -> Option<LoadedLib> {
    let t0 = crate::clock::nanos_since_boot();

    let rw_alloc = PageAlloc::new(cached.rw_size, crate::mm::pmm::Category::Elf)?;
    let src = unsafe { cached.alloc.ptr().add(cached.rw_offset) };
    unsafe {
        core::ptr::copy_nonoverlapping(src, rw_alloc.ptr(), cached.rw_size);
    }

    let t1 = crate::clock::nanos_since_boot();
    let rw_delta = rw_alloc.ptr() as i64 - (cached.alloc.ptr() as i64 + cached.rw_offset as i64);
    let image = cached.snapshot.image;
    let phys_base = image.phys();

    log!(
        "dlopen: cache hit (shared), base={:#x} {}MB total, {}MB private RW, copy={}ms",
        phys_base,
        image.size() / (1024 * 1024),
        cached.rw_size / (1024 * 1024),
        (t1 - t0) / 1_000_000
    );

    Some(cached.snapshot.into_lib(
        LibMemory::Shared {
            rw_alloc,
            cached_image: image,
            rw_offset: cached.rw_offset,
            rw_delta,
        },
        UserAddr::new(phys_base),
        Some(cached.relocs.clone()),
    ))
}
