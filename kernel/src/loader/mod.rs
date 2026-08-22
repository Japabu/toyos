//! Building a process out of an ELF file.
//!
//! `spawn` reads only headers and tables — segment contents arrive one page
//! fault at a time — so what it produces is an address space of demand-paged
//! regions plus a relocation index the fault handler applies as each page
//! arrives. Shared libraries are the exception: they are loaded eagerly into
//! kernel pages, because they are shared between processes.
//!
//! Every number the file names is untrusted. A refusal is
//! `SyscallError::{InvalidArgument, ResourceExhausted}` with a log line naming
//! the path and the field, never a panic and never a silent truncation.

// Every unsafe block under `loader::` carries a `SAFETY:` comment —
// measured and documented in full by
// `issues/build/clippy-has-never-run-here.md`'s per-area plan.
// `host-tests.yml`'s kernel clippy step already runs with `-D warnings`, so
// `warn` here is what actually gates: a new undocumented block anywhere in
// this module tree fails CI, while the rest of the kernel (not yet swept)
// stays silent.
#![warn(clippy::undocumented_unsafe_blocks)]

mod start;
mod symbols;
mod tls;

pub use start::{build_child_handles, PendingHandles, SLOT_PAIR_LEN};
pub(crate) use start::{alloc_kernel_stack, kernel_start, process_start, thread_start};
pub use tls::{setup_combined_tls, setup_tls, DTV_INITIAL_CAPACITY};

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::elf;
use crate::object::{ops, HandleTable, KObjectRef};
use crate::mm::paging::{CachePolicy, Prot};
use crate::mm::PAGE_2M;
use crate::process::{
    ElfInfo, Endowments, OwnedAlloc, PageAlloc, PageFaultTrace, PageTables, Pid,
    ProcessAccounting, ProcessData, ProcessEntry, ThreadData, ThreadEntry, UserStack,
    PROCESS_TABLE,
};
use crate::sync::Lock;
use crate::{scheduler, vfs, UserAddr};
use toyos_abi::handle::Rights;
use toyos_abi::syscall::SyscallError;
use toyos_elf::section::SectionTable;
use toyos_elf::sym::{self, SymTab};
use toyos_elf::{GnuHash, Layout};

const USER_STACK_SIZE: usize = 4 * PAGE_2M as usize; // 8 MB

/// User virtual address space starts at 1 TB — well above any direct-mapped
/// physical RAM.
const USER_VM_BASE: u64 = 0x100_0000_0000;

/// The most of a `.gnu.hash` table the loader will read off an executable.
///
/// The one table in an ELF whose extent nothing declares: no `DT_*` tag names
/// a length and the chain array ends at a bit in the data. So a read of it is
/// bounded by the containing segment's file image *and* by this, and a table
/// that outgrows the bound yields no symbol count at all — the loader falls
/// back to the `DT_SYMTAB`/`DT_STRTAB` gap rather than to a short count
/// nothing downstream could tell from a real one.
const MAX_GNU_HASH_BYTES: u64 = 64 * 1024;

/// Read a byte range from a file through the page cache.
///
/// Returns only the part of the request the file actually holds. Every `len`
/// here comes off an ELF — `DT_STRSZ`, a symbol count, `e_shnum * e_shentsize`
/// — so an unclamped `Vec::with_capacity` is a heap allocation sized by
/// untrusted input, and past EOF there is nothing to read anyway. Callers
/// treat a short return as "table truncated, stop"; they all length-check
/// before indexing.
pub(crate) fn read_file_range(
    backing: &dyn crate::file_backing::FileBacking,
    offset: u64,
    len: usize,
) -> Vec<u8> {
    let available = backing.file_size().saturating_sub(offset);
    let len = len.min(available as usize);
    let mut result = Vec::with_capacity(len);
    let mut remaining = len;
    let mut file_off = offset;
    let mut page_buf = [0u8; 4096];

    while remaining > 0 {
        let off_in_block = (file_off % 4096) as usize;
        let chunk = (4096 - off_in_block).min(remaining);

        // A page the store would not give up ends the read here rather than
        // contributing zeros. That is the same answer as EOF, and it is the
        // answer this function's callers already handle. Zeros would instead be
        // a table full of null entries, which is a different and much quieter
        // kind of wrong.
        if backing.read_page(file_off - off_in_block as u64, &mut page_buf).is_err() {
            break;
        }
        result.extend_from_slice(&page_buf[off_in_block..off_in_block + chunk]);

        file_off += chunk as u64;
        remaining -= chunk;
    }

    result
}

/// [`read_file_range`] for a length the ELF *declared* — `DT_STRSZ`,
/// `DT_RELASZ`, `e_shnum * e_shentsize`, a symbol count.
///
/// `None` above [`crate::mm::MAX_HEAP_ALLOC`], which is where the `Vec` stops
/// being an allocation failure and becomes an assert in the kernel heap's page
/// source. Refusing is deliberate: clamping the length instead would load the
/// binary with a table that is short and nothing downstream could tell.
fn read_elf_table(
    backing: &dyn crate::file_backing::FileBacking,
    offset: u64,
    len: usize,
) -> Option<Vec<u8>> {
    if len > crate::mm::MAX_HEAP_ALLOC {
        return None;
    }
    Some(read_file_range(backing, offset, len))
}

/// Whether the whole ELF image fits in the user half once it is rebased.
///
/// Every segment lands at `base + p_vaddr`, and that addition wraps: a file
/// that names a large enough `p_vaddr` places its region anywhere in the
/// machine. Two destinations matter. In the kernel half the region is still
/// demand-paged, so the first user touch reaches `AddressSpace::remap`, which
/// ORs PAGE_USER onto the page tables every process shares — the mapping
/// `sys_mmap` refuses a FIXED request for, reached through the loader instead.
/// Below `ALLOC_CEILING` it covers the arena `find_gap` serves every library,
/// TLS block and mmap out of, and there is no failure path for a process that
/// cannot be given its own TLS.
///
/// One range check covers every segment, because `[vaddr_min, vaddr_max)`
/// covers every segment. The bound is the hardware's user/kernel split, not a
/// policy number.
fn image_fits_user_half(layout: &Layout) -> bool {
    toyos_userbound::in_user_half(USER_VM_BASE, layout.span())
}

/// Insert one demand-paged region per `PT_LOAD` segment.
///
/// `Err` when two segments would claim the same page. A segment's regions are
/// page-rounded, so segments that merely *share* a page are two regions at one
/// address and `insert_region` asserts — a kernel-bug assert reached from a
/// file. Asked before the first insert, so a refusal leaves the address space
/// as it found it.
fn insert_elf_regions(
    addr_space: &mut crate::mm::paging::AddressSpace,
    layout: &Layout,
    base: u64,
    backing: &Arc<dyn crate::file_backing::FileBacking>,
) -> Result<(), SyscallError> {
    use crate::vma::{Region, RegionKind};

    if let Some((a, b)) = layout.overlapping_load_pages(4096) {
        log!("spawn: PT_LOAD segments {} and {} contend for a page", a, b);
        return Err(SyscallError::InvalidArgument);
    }

    for seg in layout.segments() {
        let (lo, hi) = seg.page_range(layout.vaddr_min, 4096);
        let (seg_start, seg_end) = (base + layout.vaddr_min + lo, base + layout.vaddr_min + hi);
        // A segment that covers no page maps nothing, and a zero-size region
        // would sit in the map where `find_region` cannot see past it.
        if seg_end == seg_start {
            continue;
        }
        let prot = segment_prot(seg);

        let file_block_start = seg.file_offset / 4096;
        let file_blocks_needed = (seg.filesz + (seg.file_offset % 4096)).div_ceil(4096);
        let file_backed_end = seg_start + file_blocks_needed * 4096;

        if file_blocks_needed > 0 {
            addr_space.insert_region(
                UserAddr::new(seg_start),
                Region {
                    size: file_backed_end.min(seg_end) - seg_start,
                    kind: RegionKind::FileBacked {
                        backing: Arc::clone(backing),
                        file_offset: file_block_start * 4096,
                        file_size: seg.filesz + (seg.file_offset % 4096),
                        prot,
                    },
                },
            );
        }

        if file_backed_end < seg_end {
            let anon_start = file_backed_end.max(seg_start);
            addr_space.insert_region(
                UserAddr::new(anon_start),
                Region {
                    size: seg_end - anon_start,
                    kind: RegionKind::Anonymous { prot },
                },
            );
        }
    }
    Ok(())
}

/// What one `PT_LOAD` segment's pages may be used for.
///
/// **A file that declares itself both writable and executable is refused the
/// write, not the execution.** `PF_W | PF_X` is a segment no linker in this
/// tree emits and no correct program needs, and there is no fourth [`Prot`] to
/// honour it with; taking `X` away instead would turn a hostile ELF into a
/// process that runs its own data, while taking `W` away leaves it faulting on
/// the first store. `PF_R` alone — a `.rodata` segment — is the plain
/// [`Prot::Read`] case, and a segment claiming nothing at all gets it too.
fn segment_prot(seg: &toyos_elf::Segment) -> crate::mm::paging::Prot {
    use crate::mm::paging::Prot;
    if seg.flags.executable() {
        Prot::ReadExec
    } else if seg.flags.writable() {
        Prot::ReadWrite
    } else {
        Prot::Read
    }
}

/// Everything the executable's `PT_DYNAMIC` names, resolved to file offsets and
/// read.
///
/// One value rather than a dozen locals threaded through `spawn`: the file
/// offsets, the two symbol tables and the relocation entries were each derived
/// twice from the same tags, and one of those derivations built a symbol map
/// that nothing read.
struct ExeTables {
    /// `DT_NEEDED` values, as offsets into `dynstr`. Collected while the
    /// dynamic table is still in hand, and reserved exactly: a table with no
    /// `DT_NULL` runs to the end of the buffer and every entry in it can be a
    /// `DT_NEEDED`, which is `len / 16` — half an input that was already
    /// bounded, rather than a doubling sequence whose last step is larger than
    /// the input.
    needed: Vec<u64>,
    dynstr: Vec<u8>,
    dynsym: Vec<u8>,
    symtab_file_off: Option<u64>,
    relas: elf::ParsedRelaEntries,
}

impl ExeTables {
    fn symbols(&self) -> SymTab<'_> {
        SymTab::new(&self.dynsym, &self.dynstr)
    }

    /// One symbol read straight off the file.
    ///
    /// A relocation may name an index past the `.dynsym` the loader read: that
    /// count came from `.gnu.hash` or the gap between two tags, and both can
    /// understate the table. The relocation's own index is the other evidence
    /// about how long it is, so the record is fetched rather than the count
    /// trusted.
    fn symbol(&self, backing: &dyn crate::file_backing::FileBacking, r_sym: u32) -> Option<toyos_elf::Sym> {
        let off = self
            .symtab_file_off?
            .checked_add(r_sym as u64 * sym::ENTRY_SIZE as u64)?;
        sym::parse_at(&read_file_range(backing, off, sym::ENTRY_SIZE), 0)
    }
}

/// Turn a `DT_*` vaddr into a file offset, or refuse the binary.
///
/// There is no answer for a vaddr that lies below every `PT_LOAD`, and the
/// alternative this replaced was a panic in syscall context.
fn file_off(layout: &Layout, path: &str, what: &str, vaddr: u64) -> Result<u64, SyscallError> {
    match layout.vaddr_to_file_offset(vaddr) {
        Some(off) => Ok(off),
        None => {
            log!("spawn: {}: {} vaddr {:#x} is in or near no PT_LOAD segment", path, what, vaddr);
            Err(SyscallError::InvalidArgument)
        }
    }
}

/// Read a table whose length the file declared, or refuse the binary.
///
/// Above one kernel allocation the `Vec` is a panic in the heap's page source,
/// so it is a refusal here.
fn table(
    backing: &dyn crate::file_backing::FileBacking,
    path: &str,
    what: &str,
    off: u64,
    len: usize,
) -> Result<Vec<u8>, SyscallError> {
    match read_elf_table(backing, off, len) {
        Some(v) => Ok(v),
        None => {
            log!("spawn: {}: {} declares {} bytes, past one kernel allocation", path, what, len);
            Err(SyscallError::ResourceExhausted)
        }
    }
}

fn read_exe_tables(
    backing: &dyn crate::file_backing::FileBacking,
    layout: &Layout,
    path: &str,
) -> Result<ExeTables, SyscallError> {
    let (dyn_info, needed) = match layout.dynamic {
        Some((dyn_off, _, dyn_size)) => {
            let data = table(backing, path, "PT_DYNAMIC", dyn_off, dyn_size as usize)?;
            let mut needed = Vec::new();
            needed.reserve_exact(data.len() / toyos_elf::dynamic::ENTRY_SIZE);
            needed.extend(toyos_elf::Dynamic::needed(&data));
            (toyos_elf::Dynamic::parse(&data), needed)
        }
        None => (toyos_elf::Dynamic::default(), Vec::new()),
    };

    let rela_data = match dyn_info.rela {
        Some(t) => {
            let off = file_off(layout, path, "DT_RELA", t.vaddr)?;
            table(backing, path, "DT_RELASZ", off, t.size as usize)?
        }
        // No PT_DYNAMIC at all: `.rela.dyn` is found through the section
        // headers instead, by shape rather than by name.
        None if layout.dynamic.is_none() => rela_dyn_from_sections(backing, layout, path)?,
        None => Vec::new(),
    };
    let jmprel_data = match dyn_info.jmprel {
        Some(t) => {
            let off = file_off(layout, path, "DT_JMPREL", t.vaddr)?;
            table(backing, path, "DT_PLTRELSZ", off, t.size as usize)?
        }
        None => Vec::new(),
    };
    let Some(relas) = elf::parse_rela_entries(&rela_data, &jmprel_data) else {
        log!("spawn: {}: relocation tables do not fit one allocation", path);
        return Err(SyscallError::ResourceExhausted);
    };

    let dynstr = match dyn_info.strtab_table() {
        Some(t) => {
            let off = file_off(layout, path, "DT_STRTAB", t.vaddr)?;
            table(backing, path, "DT_STRSZ", off, t.size as usize)?
        }
        None => Vec::new(),
    };

    let symtab_file_off = match dyn_info.symtab {
        Some(vaddr) => Some(file_off(layout, path, "DT_SYMTAB", vaddr)?),
        None => None,
    };
    let sym_count = exe_sym_count(backing, layout, &dyn_info, path)?;
    let dynsym = match (symtab_file_off, sym_count) {
        (Some(off), n) if n > 0 => table(backing, path, "symbol count", off, n * sym::ENTRY_SIZE)?,
        _ => Vec::new(),
    };

    Ok(ExeTables { needed, dynstr, dynsym, symtab_file_off, relas })
}

/// `.dynsym`'s entry count, from `.gnu.hash` if the file has one and from the
/// `DT_SYMTAB`-to-`DT_STRTAB` gap otherwise.
fn exe_sym_count(
    backing: &dyn crate::file_backing::FileBacking,
    layout: &Layout,
    dyn_info: &toyos_elf::Dynamic,
    path: &str,
) -> Result<usize, SyscallError> {
    if let Some(vaddr) = dyn_info.gnu_hash {
        let off = file_off(layout, path, "DT_GNU_HASH", vaddr)?;
        let len = layout
            .file_bytes_from(vaddr)
            .unwrap_or(0)
            .min(MAX_GNU_HASH_BYTES) as usize;
        let data = read_file_range(backing, off, len);
        if let Some(count) = GnuHash::parse(&data).and_then(|h| h.sym_count()) {
            return Ok(count);
        }
        log!("spawn: {}: .gnu.hash does not describe a symbol count in {} bytes", path, len);
    }
    // The two tables are adjacent in every layout a linker produces, so the gap
    // between them is the symbol table's size.
    match (dyn_info.symtab, dyn_info.strtab) {
        (Some(symtab), Some(strtab)) if strtab > symtab => {
            Ok(((strtab - symtab) / sym::ENTRY_SIZE as u64) as usize)
        }
        _ => Ok(0),
    }
}

/// `.rela.dyn` located through the section headers, for a file with no
/// `PT_DYNAMIC`.
fn rela_dyn_from_sections(
    backing: &dyn crate::file_backing::FileBacking,
    layout: &Layout,
    path: &str,
) -> Result<Vec<u8>, SyscallError> {
    let Some(sections) = layout.section_headers else {
        return Ok(Vec::new());
    };
    let shdrs = table(backing, path, "e_shnum", sections.file_offset, sections.byte_len())?;
    let mut first = |off: u64| {
        let head = read_file_range(backing, off, toyos_elf::rela::ENTRY_SIZE);
        toyos_elf::RelaTable::new(&head).get(0)
    };
    match SectionTable::new(&shdrs).rela_dyn(&mut first) {
        Some((off, size)) => table(backing, path, "SHT_RELA sh_size", off, size as usize),
        None => Ok(Vec::new()),
    }
}

/// Load a program and place its main thread, answering the object a handle to
/// the new process names.
///
/// **No parent argument, because a process has no parent.** The one thing the
/// spawning process contributed beyond what it endowed was its working
/// directory, and the caller passes that: nothing else about the caller is
/// recorded, so there is no relationship for a later call to authorize
/// against.
/// **The refusal is a value, and that is what stops eight megabytes going with
/// it.** Every failure below owns a partly built process — the child's address
/// space with its ELF regions mapped, `USER_STACK_SIZE` of stack, a 128 KiB
/// kernel stack, the symbol table, the loaded libraries — and nothing unwinds,
/// so a `-> !` taken from inside this frame strands all of it. `Refusal` is
/// therefore the error type: the three handle kinds that end the caller are
/// carried out to the dispatcher and refused there, with this frame gone. It is
/// the same shape `c29bb8a` fixed one layer up, where the stranded values were
/// three `Arc`s rather than 8 MB.
pub fn spawn(
    argv: &[&str],
    pending: PendingHandles,
    cwd: String,
    env: Vec<u8>,
) -> Result<Arc<crate::object::process::ProcessObject>, crate::object::Refusal> {
    // An argv of only separators survives the split in sys_spawn as an empty
    // slice; there is no argv[0] to load.
    let Some(&path) = argv.first() else {
        return Err(SyscallError::InvalidArgument.into());
    };
    let t0 = crate::clock::nanos_since_boot();

    // The guard is scoped rather than held across the match: every `return`
    // past this point drops `pending`, and releasing a file object takes the
    // VFS lock (`object::file::OpenFileState::drop`).
    let opened = vfs::lock().open_backing(path);
    let backing: Arc<dyn crate::file_backing::FileBacking> = match opened {
        Ok(b) => b,
        Err(e) => {
            log!("spawn: {}: {e}", path);
            return Err(e.into());
        }
    };

    let header_size = 4096.min(backing.file_size() as usize);
    let header_data = read_file_range(backing.as_ref(), 0, header_size);
    let layout = match elf::parse_layout(&header_data) {
        Ok(l) => l,
        Err(msg) => {
            log!("spawn: {}: {}", path, msg);
            return Err(SyscallError::InvalidArgument.into());
        }
    };

    if !image_fits_user_half(&layout) {
        log!("spawn: {}: image spans {:#x} bytes, past the user half from {:#x}",
            path, layout.span(), USER_VM_BASE);
        return Err(SyscallError::InvalidArgument.into());
    }
    let base = USER_VM_BASE - layout.vaddr_min;

    let exe = read_exe_tables(backing.as_ref(), &layout, path)?;
    let t1 = crate::clock::nanos_since_boot();

    // Reserved from the counts rather than grown: `add_u64` is called for every
    // RELATIVE and TPOFF64 entry and for each GLOB_DAT that resolves, so these
    // are exact upper bounds and nothing here reallocates.
    let u64_writes =
        exe.relas.relative.len() + exe.relas.glob_dat.len() + exe.relas.tpoff64.len();
    let Some(mut reloc_index) =
        elf::RelocationIndex::with_capacity(u64_writes, exe.relas.tpoff32.len())
    else {
        log!("spawn: {}: {} relocations do not fit one index", path, u64_writes);
        return Err(SyscallError::ResourceExhausted.into());
    };
    for &(r_offset, r_addend) in &exe.relas.relative {
        reloc_index.add_u64(r_offset, (base as i64 + r_addend) as u64);
    }

    let t2 = crate::clock::nanos_since_boot();
    let mut loaded_libs = load_needed_libs(&exe, path)?;
    let t_deps = crate::clock::nanos_since_boot();

    // ELF segments are demand-faulted; the address space starts empty.
    let child_pt: PageTables = Arc::new(Lock::new(crate::mm::paging::AddressSpace::new_user()));
    insert_elf_regions(&mut child_pt.lock(), &layout, base, &backing)?;

    // Libraries are mapped and given their user addresses *before* any
    // relocation is written, because a GOT entry's value is an address in this
    // process: RELATIVE is `user_base + addend` and GLOB_DAT is
    // `user_base + st_value`.
    map_libs(&child_pt, &mut loaded_libs, path)?;
    for lib in &loaded_libs.libs {
        let delta = lib.user_base.raw() as i64 - lib.phys_base as i64;
        if delta != 0 {
            elf::rebase_relative_relocs(lib, delta);
        }
    }

    if !loaded_libs.libs.is_empty() {
        // A PIE linked without `--export-dynamic` exports nothing through
        // `.dynsym`, and `.symtab` is where its symbols then are. Read only
        // when the first map came back empty: it is the whole symbol table of
        // the binary, where `.dynsym` is only what it meant to export.
        let fallback = exe
            .symbols()
            .defined()
            .next()
            .is_none()
            .then(|| symbols::read_symtab(backing.as_ref(), &layout))
            .flatten();
        let exe_sym_map = match &fallback {
            Some((syms, strs)) => {
                symbols::static_map(&SymTab::new(syms, strs), UserAddr::new(base))
            }
            None => symbols::dynamic_map(&exe.symbols(), UserAddr::new(base)),
        };
        log!("dynamic: {} exe symbols available to libraries", exe_sym_map.len());
        for lib in &loaded_libs.libs {
            elf::resolve_lib_bind_relocs(lib, &exe_sym_map, &loaded_libs.libs);
        }

        // The exe's own imports, resolved against the libraries it just got.
        for &(r_offset, r_sym, _) in &exe.relas.glob_dat {
            if r_sym == 0 {
                continue;
            }
            let Some(sym) = exe.symbol(backing.as_ref(), r_sym) else {
                continue;
            };
            let name = toyos_elf::cstr(&exe.dynstr, sym.name as u64);
            match loaded_libs.libs.iter().find_map(|lib| lib.resolve(name)) {
                Some(addr) => reloc_index.add_u64(r_offset, addr.raw()),
                None => log!("dynamic: unresolved exe symbol: {}", name),
            }
        }
    }

    // The stack is at a fixed virtual address and is mapped eagerly: every
    // process touches it immediately.
    let stack_pages = match PageAlloc::new(USER_STACK_SIZE, crate::mm::pmm::Category::Stack) {
        Some(a) => a,
        None => {
            log!("spawn: {}: failed to allocate user stack ({} bytes)", path, USER_STACK_SIZE);
            return Err(SyscallError::ResourceExhausted.into());
        }
    };
    let stack_vaddr = UserAddr::new(crate::vma::STACK_BASE);
    // The window is the allocation's own, so `USER_STACK_SIZE` is named once
    // here — at the `PageAlloc::new` above — and every argv write is bounded
    // against what was actually allocated rather than against a second copy of
    // the constant.
    let user_stack = UserStack::new(stack_vaddr, stack_pages.window());
    {
        let mut pt = child_pt.lock();
        // The stack is data, and `Prot::ReadWrite` is what makes it stop being
        // a place to jump to: eight megabytes of writable, executable memory at
        // a fixed address is the shape every stack-smashing payload is written
        // against.
        pt.map_range(stack_vaddr, stack_pages.phys(), USER_STACK_SIZE as u64,
            Prot::ReadWrite, CachePolicy::DeferToMtrr);
        pt.insert_region(stack_vaddr, crate::vma::Region {
            size: USER_STACK_SIZE as u64,
            kind: crate::vma::RegionKind::Anonymous { prot: Prot::ReadWrite },
        });
    }

    let exe_tls_template = match layout.tls.filter(|t| t.memsz > 0) {
        Some(tls) => {
            let tls_file_off = file_off(&layout, path, "PT_TLS", tls.vaddr)?;
            // Read straight into the `memsz`-sized buffer rather than via an
            // intermediate `Vec` sized by `filesz`. `OwnedAlloc` zeroes, so the
            // `.tbss` tail needs no second pass — and it refuses a size past
            // one heap allocation itself, so there is no second copy of that
            // ceiling here.
            let Some(tls_buf) = OwnedAlloc::new(tls.memsz as usize, 16) else {
                log!("spawn: {}: cannot allocate a {}-byte TLS template", path, tls.memsz);
                return Err(SyscallError::ResourceExhausted.into());
            };
            // `slice` bounds `filesz` against the `memsz`-sized buffer it was
            // taken from, so the `filesz <= memsz` that `Layout::parse` refuses
            // a `PT_TLS` without is now checked here too — against the
            // allocation, rather than argued from the parser two crates away.
            if elf::read_backing_into(
                backing.as_ref(),
                tls_file_off,
                tls_buf.slice(tls.filesz as usize),
            )
            .is_err()
            {
                log!("spawn: {}: the TLS template could not be read off the device", path);
                return Err(SyscallError::NotFound.into());
            }
            Some(tls_buf)
        }
        None => None,
    };

    let Some((tls_modules, tls_total_memsz, max_tls_align, next_tls_module_id)) =
        tls::build_tls_layout(&loaded_libs.libs, &layout, exe_tls_template.as_ref())
    else {
        log!("spawn: {}: the TLS modules do not fit one block", path);
        return Err(SyscallError::ResourceExhausted.into());
    };

    apply_tls_relocs(&exe, backing.as_ref(), &loaded_libs.libs, &tls_modules,
        tls_total_memsz, &mut reloc_index);

    reloc_index.finalize();
    let reloc_index = if reloc_index.len() > 0 {
        log!("ELF: {} relocations indexed (RELATIVE + GLOB_DAT + TPOFF)", reloc_index.len());
        Some(Arc::new(reloc_index))
    } else {
        None
    };

    let (tls_template, tls_memsz) = match tls_modules.first() {
        Some(m) => (m.template, m.memsz),
        None => (None, 0),
    };

    log!("spawn: TLS {} modules, total_memsz={}", tls_modules.len(), tls_total_memsz);
    let Some((tls_pages, fs_base)) =
        tls::map_block(&child_pt, &tls_modules, tls_total_memsz, max_tls_align)
    else {
        log!("spawn: {}: failed to allocate TLS ({} bytes)", path, tls_total_memsz);
        return Err(SyscallError::ResourceExhausted.into());
    };

    let entry = base + layout.entry;
    let sp = user_stack.write_argv(argv);
    let t_tls = crate::clock::nanos_since_boot();

    let syms = symbols::read_backtrace_table(
        backing.as_ref(), &layout, path, base,
        base + layout.vaddr_min, base + layout.vaddr_max,
        user_stack.base().raw(), user_stack.top(),
    );
    let sym_bytes = syms.resident_bytes();

    let (ks_alloc, ks_rsp) = match alloc_kernel_stack(process_start, entry, sp, 0) {
        Some(ks) => ks,
        None => {
            log!("spawn: {}: failed to allocate kernel stack", path);
            return Err(SyscallError::ResourceExhausted.into());
        }
    };


    let NeededLibs { libs: loaded_libs, paths: lib_paths } = loaded_libs;
    // **The point of no return, and the last thing that can refuse.** Every
    // failure above this line answers the caller with its table untouched; the
    // endowed handles leave it here, where nothing between this and the process
    // entry can fail.
    // A handle that stopped resolving between the arguments being read and this
    // line is the caller racing its own spawn: a bug in the caller, and fatal.
    // It travels out as a value so this frame's eight megabytes drop on the
    // way, and the dispatcher ends the caller with nothing held.
    let (handles, endowments) = pending.commit()?;
    let proc_data = Arc::new(Lock::new(ProcessData {
        handles,
        cwd,
        env,
        elf: ElfInfo {
            elf_alloc: exe_tls_template,
            tls_template,
            tls_memsz,
            tls_modules,
            tls_total_memsz,
            tls_max_align: max_tls_align,
            next_tls_module_id,
            dynamic_tls_blocks: alloc::collections::BTreeMap::new(),
            loaded_libs,
            reloc_index,
            elf_base: UserAddr::new(base),
            exe_eh_frame_hdr_vaddr: layout.eh_frame_hdr.map_or(0, |(v, _)| v),
            exe_eh_frame_hdr_size: layout.eh_frame_hdr.map_or(0, |(_, s)| s),
            exe_vaddr_max: base + layout.vaddr_max,
            lib_paths,
        },
        mmap_regions: Vec::new(),
        pipe_maps: Vec::new(),
        demand_pages: Vec::new(),
        fault_trace: PageFaultTrace::new(),
        peak_memory: 0,
        alloc_count: 0,
        free_count: 0,
        exe_path: String::from(path),
        spawn_ns: crate::clock::nanos_since_boot(),
        accounting: ProcessAccounting::default(),
        endowments,
    }));

    let thread_data = Arc::new(Lock::new(ThreadData {
        tls_pages: Some(tls_pages),
        stack_pages: Some(stack_pages),
        user_stack_base: user_stack.base(),
        user_stack_size: user_stack.size(),
        syscall_counts: [0; toyos_abi::syscall::SYSCALL_PROFILE_BINS],
        syscall_total: 0,
        syscall_total_ns: 0,
    }));

    // One table, two holders: the entry owns it, and the main thread's task
    // record carries a clone so a crash report on that thread reads the names
    // without asking the process table (`process`'s module header).
    let syms = Arc::new(syms);

    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().unwrap();
    let pid = table.insert_with(|pid| ProcessEntry::new(
        pid,
        start::make_name(path),
        proc_data,
        Arc::clone(&syms),
        ThreadEntry::new(thread_data),
    ));
    let tid = table.get(pid).unwrap().main_tid();
    let object = Arc::clone(table.get(pid).unwrap().object());

    // Placed while still holding the table lock: kill_process claims teardown
    // under this lock, so once the pid is visible its main thread is already in
    // the scheduler — a retire sweep can never miss it in a table-insert→place
    // gap.
    let sched = scheduler::enqueue_new(
        scheduler::TaskId(pid, tid),
        ks_alloc,
        ks_rsp,
        child_pt.clone(),
        fs_base,
        syms,
    );
    table.get_mut(pid).unwrap().threads_mut().get_mut(tid).unwrap().set_sched(sched);
    drop(guard);

    let t3 = crate::clock::nanos_since_boot();
    log!("spawn: {} pid={} tid={} base={:#x} entry={:#x} cr3={:#x} symbols={}KiB (layout={}ms relocs={}ms deps={}ms tls={}ms total={}ms)",
        path, pid, tid, base, entry, child_pt.lock().cr3().phys(), sym_bytes / 1024,
        (t1 - t0) / 1_000_000, (t2 - t1) / 1_000_000, (t_deps - t2) / 1_000_000,
        (t_tls - t_deps) / 1_000_000, (t3 - t0) / 1_000_000);

    Ok(object)
}

/// The libraries an executable's `DT_NEEDED` entries name, and the paths they
/// were found at.
struct NeededLibs {
    libs: Vec<elf::LoadedLib>,
    paths: Vec<String>,
}

/// Load every `DT_NEEDED` library, from the executable's own directory first
/// and `/lib` second.
fn load_needed_libs(exe: &ExeTables, path: &str) -> Result<NeededLibs, SyscallError> {
    let mut out = NeededLibs { libs: Vec::new(), paths: Vec::new() };
    if exe.needed.is_empty() {
        return Ok(out);
    }
    let exe_dir = path.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");

    for &name_offset in &exe.needed {
        // An offset the string table does not hold is an empty name, not a
        // bounds failure: nothing is named, so nothing is loaded.
        let lib_name = toyos_elf::cstr(&exe.dynstr, name_offset);
        if lib_name.is_empty() {
            continue;
        }
        let lib_path = alloc::format!("{}/{}", exe_dir, lib_name);
        let t_load0 = crate::clock::nanos_since_boot();

        if let Some(lib) = elf::try_clone_cached(&lib_path) {
            out.paths.push(lib_path);
            out.libs.push(lib);
            continue;
        }

        // The fallback answers a library that is not where the binary said it
        // was, and nothing else: a mount that refused the first lookup would
        // refuse the second, and trying it again turns one device failure into
        // two log lines and the wrong verdict.
        let so_backing = {
            let b = vfs::lock().open_backing(&lib_path);
            match b {
                Ok(b) => b,
                Err(SyscallError::NotFound) => {
                    let fallback = alloc::format!("/lib/{}", lib_name);
                    match vfs::lock().open_backing(&fallback) {
                        Ok(b) => b,
                        Err(e) => {
                            log!("spawn: {}: failed to load {}: {e}", path, lib_name);
                            return Err(e);
                        }
                    }
                }
                Err(e) => {
                    log!("spawn: {}: failed to load {}: {e}", path, lib_name);
                    return Err(e);
                }
            }
        };

        match elf::load_shared_lib(so_backing.as_ref()) {
            Ok((lib, rw_offset, rw_size)) => {
                let t_load1 = crate::clock::nanos_since_boot();
                log!("dynamic: loaded {} base={:#x} ({} syms, {}ms)",
                    lib_name, lib.phys_base, lib.sym_count(), (t_load1 - t_load0) / 1_000_000);
                out.libs.push(elf::cache_loaded_lib(&lib_path, lib, rw_offset, rw_size));
                out.paths.push(lib_path);
            }
            Err(e) => {
                log!("spawn: {}: failed to load {}: {}", path, lib_name, e);
                return Err(SyscallError::NotFound);
            }
        }
    }
    Ok(out)
}

/// Give every library a virtual address in the child and map its pages there.
///
/// A cached library's read-only pages are the cache's own, mapped into this
/// process; only its writable window is private. Which pages of it may be
/// written and which may be executed is [`LoadedLib::map_into`]'s, out of the
/// module's own program headers.
///
/// [`LoadedLib::map_into`]: crate::elf::LoadedLib::map_into
fn map_libs(
    child_pt: &PageTables,
    loaded: &mut NeededLibs,
    path: &str,
) -> Result<(), SyscallError> {
    for lib in &mut loaded.libs {
        let Some(vaddr) = lib.map_into(child_pt) else {
            log!("spawn: {}: out of virtual address space for a library", path);
            return Err(SyscallError::ResourceExhausted);
        };
        lib.user_base = vaddr;
    }
    Ok(())
}

/// Apply the TLS relocations of every startup library, and index the
/// executable's.
///
/// The libraries' land in their images directly; the executable's go into the
/// relocation index, because its pages do not exist yet.
fn apply_tls_relocs(
    exe: &ExeTables,
    backing: &dyn crate::file_backing::FileBacking,
    loaded_libs: &[elf::LoadedLib],
    tls_modules: &[elf::TlsModule],
    tls_total_memsz: usize,
    reloc_index: &mut elf::RelocationIndex,
) {
    let tls_info = elf::TlsModuleInfo { libs: loaded_libs, modules: tls_modules };
    for lib in loaded_libs {
        // Matched by template pointer — unique per lib, since each points into
        // a distinct image. A lib without TLS has no template and matches
        // nothing.
        let module = tls_modules.iter().find(|m| m.template == lib.tls_template);
        let base_offset = module.map_or(0, |m| m.base_offset);
        // Initial-exec: references to TLS in the static block.
        elf::apply_tpoff_relocs(lib, base_offset, tls_total_memsz, &tls_info);
        // General-dynamic: this lib's own TLS, reached through the DTV.
        if let Some(m) = module {
            elf::apply_dtpmod_relocs(lib, m.module_id, &tls_info);
        }
    }

    let exe_base_offset = tls_modules
        .iter()
        .find(|m| m.module_id == 1)
        .map_or(0, |m| m.base_offset);
    for &(r_offset, r_sym, r_addend) in &exe.relas.tpoff64 {
        let tpoff = exe_tpoff(exe, backing, r_sym, r_addend, exe_base_offset,
            tls_total_memsz, &tls_info);
        reloc_index.add_u64(r_offset, tpoff as u64);
    }
    for &(r_offset, r_sym, r_addend) in &exe.relas.tpoff32 {
        let tpoff = exe_tpoff(exe, backing, r_sym, r_addend, exe_base_offset,
            tls_total_memsz, &tls_info);
        reloc_index.add_i32(r_offset, tpoff as i32);
    }
}

/// One of the executable's `TPOFF` relocations, resolved to a value.
///
/// A symbol the file does not hold resolves as the unnamed form, which is the
/// same answer as `r_sym == 0`: there is nothing to resolve against, so the
/// module-relative offset is all that is left.
#[allow(clippy::too_many_arguments)]
fn exe_tpoff(
    exe: &ExeTables,
    backing: &dyn crate::file_backing::FileBacking,
    r_sym: u32,
    r_addend: i64,
    exe_base_offset: usize,
    total_memsz: usize,
    tls_info: &elf::TlsModuleInfo,
) -> i64 {
    let unnamed = exe_base_offset as i64 + r_addend - total_memsz as i64;
    if r_sym == 0 {
        return unnamed;
    }
    let Some(sym) = exe.symbol(backing, r_sym) else {
        return unnamed;
    };
    if sym.is_defined() {
        return exe_base_offset as i64 + sym.value as i64 + r_addend - total_memsz as i64;
    }

    let name = toyos_elf::cstr(&exe.dynstr, sym.name as u64);
    for lib in tls_info.libs {
        if lib.tls_memsz == 0 {
            continue;
        }
        if let Some(sym_tls_offset) = lib.resolve_tls(name) {
            let other_base = tls_info
                .modules
                .iter()
                .find(|m| m.template == lib.tls_template)
                .map_or(0, |m| m.base_offset);
            return other_base as i64 + sym_tls_offset as i64 - total_memsz as i64;
        }
    }
    log!("tpoff: unresolved exe TLS symbol: {}", name);
    0
}

/// The one program the kernel starts. `src/build.rs` puts this binary in every
/// image regardless of `[programs]`, so an initrd without it is a build that
/// went wrong rather than a machine that boots differently.
pub const INIT_PATH: &str = "/bin/init";

/// Start `/bin/init`, holding the machine's one full-rights `SysCap`.
///
/// Nothing else can construct one, so the set of processes that can ever mint
/// a device claim, enter the RT band, open a process by pid, list every process
/// in the machine, or power the machine off is exactly what init endows. Panics
/// on failure: a boot that cannot start init has nowhere to report to and
/// nothing left to do.
pub fn spawn_init() -> Pid {
    let mut handles = HandleTable::new();
    let console = KObjectRef::Console(crate::object::device::ConsoleObject::new());
    for slot in 0..3 {
        let entry = crate::object::HandleEntry::new(
            console.clone(),
            ops::initial_rights(&console),
        );
        let (_, displaced) = handles
            .install_at(slot, entry)
            .expect("spawn_init: three slots cannot exhaust an empty table");
        assert!(displaced.is_none(), "an empty table had something at slot {slot}");
    }
    let cap = KObjectRef::SysCap(crate::object::syscap::SysCap::new());
    // **Every machine-wide authority this system has, because this is the root
    // of the tree.** Rights only shrink, so a bit absent here is a bit no
    // manifest can ever name. `LOG` and `WAIT` arrive together and are one
    // name (`toyos_manifest`'s `logread`): reading every record every CPU wrote,
    // and parking on the source that says there are more. `SYS_LOG_READ` never
    // blocks by design, so a holder given the first without the second would
    // have to spin. `ROSTER` is alone for the opposite reason: `SYS_SYSINFO`
    // answers where it stands, so its holder has nothing to park on.
    let rights = Rights::DUP
        .union(Rights::TRANSFER)
        .union(Rights::DEVICE)
        .union(Rights::RT)
        .union(Rights::MANAGE)
        .union(Rights::LOG)
        .union(Rights::WAIT)
        .union(Rights::POWER)
        .union(Rights::ROSTER);
    let cap_handle = handles
        .install(crate::object::HandleEntry::new(cap, rights))
        .expect("spawn_init: an empty table refused the system capability");
    let label = toyos_abi::syscall::SYSCAP_LABEL;
    let endowments = Endowments::new(
        alloc::vec![toyos_abi::syscall::EndowEntry {
            label_off: 0,
            label_len: label.len() as u32,
            handle: cap_handle,
            _pad: 0,
        }],
        label.as_bytes().to_vec(),
    );
    match spawn(&[INIT_PATH], PendingHandles::Ready(handles, endowments), String::from("/"), Vec::new()) {
        Ok(object) => object.pid(),
        Err(crate::object::Refusal::Error(e)) => panic!("spawn_init: failed to spawn: {e:?}"),
        Err(crate::object::Refusal::Handle(e)) => panic!("spawn_init: {e}"),
    }
}
