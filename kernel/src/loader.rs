use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::arch::naked_asm;
use crate::elf;
use crate::mm::paging::CachePolicy;
use crate::fd::{Descriptor, FdTable};
use crate::sync::Lock;
use crate::symbols::SymbolTable;
use crate::{scheduler, vfs, DirectMap, UserAddr};
use crate::mm::PAGE_2M;
use crate::process::{
    Pid, ProcessEntry, ThreadEntry, ProcessData, ElfInfo, ThreadData,
    OwnedAlloc, PageAlloc, UserStack, PageTables, PageFaultTrace, ProcessAccounting,
    PROCESS_TABLE, KERNEL_STACK_SIZE, vma_map, fd_owner_data,
};
use toyos_abi::syscall::SyscallError;

const USER_STACK_SIZE: usize = 4 * PAGE_2M as usize; // 8 MB

const TCB_SIZE: usize = 64;
/// Initial DTV capacity (number of module entries).
pub const DTV_INITIAL_CAPACITY: usize = 64;
/// Header size: generation (8) + len (8).
const DTV_HEADER_SIZE: usize = 16;
/// Sentinel value for unallocated DTV entries.
const DTV_UNALLOCATED: u64 = !0u64;

/// Allocate a TLS area using the x86-64 variant II layout:
/// [TLS data (.tdata + .tbss)] [TCB: self-pointer]
///                              ^-- FS base (thread pointer)
/// Returns (alloc, fs_base).
pub fn setup_tls(tls_template: Option<crate::mm::KernelSlice>, tls_memsz: usize, tls_align: usize) -> Option<(PageAlloc, u64)> {
    setup_combined_tls(&[elf::TlsModule { template: tls_template, memsz: tls_memsz, base_offset: 0, module_id: 1, is_static: true }], tls_memsz, tls_align)
}

/// Allocate a combined TLS area for multiple modules (exe + shared libraries).
/// Each module's template is copied at its base_offset within the block.
///
/// x86-64 TLS Variant II layout:
///   [DTV] [alignment padding] [TLS data (.tdata + .tbss)] [TCB (64 bytes)]
///                                                          ^-- TP (FS base)
///
/// The linker (LLD) computes TPOFF = sym_offset - memsz (raw, NOT rounded).
/// TP must be placed at data_start + memsz to match.
/// data_start must be aligned to tls_align so variable offsets work correctly.
///
/// TCB layout:
///   TP+0x00: self-pointer (fs:[0] == &TCB, x86_64 ABI requirement)
///   TP+0x08: DTV pointer (user-visible physical address of DTV)
///   TP+0x10..0x3F: reserved (zero)
///
/// DTV layout (at start of allocation):
///   [0x00] generation: u64
///   [0x08] len: u64 (max module_id this DTV can hold)
///   [0x10] entries[0]: u64 (pointer for module_id=1)
///   [0x18] entries[1]: u64 (pointer for module_id=2)
///   ...
///
/// `None` for a layout no allocation can hold. Both `total_memsz` and
/// `tls_align` are sums of file-declared numbers, so every step of the sizing
/// is checked: the DTV, the alignment padding and the block itself all have to
/// fit, and the round-up to 2 MiB must not wrap. `parse_layout` has already
/// established that `tls_align` is a power of two no larger than a 2 MiB page,
/// which is what makes `!(align - 1)` a mask.
pub fn setup_combined_tls(
    modules: &[crate::elf::TlsModule],
    total_memsz: usize,
    tls_align: usize,
) -> Option<(PageAlloc, u64)> {
    let dtv_size = DTV_HEADER_SIZE + DTV_INITIAL_CAPACITY * 8;
    let align = if tls_align > 1 { tls_align } else { 8 };
    let block_size = total_memsz.checked_add(TCB_SIZE)?;
    // The DTV goes at the start of this same allocation and the TLS data is
    // placed `align`-aligned above it, so both belong in the size. Sizing from
    // the block and the alignment alone left `tls_start` free to land inside
    // the DTV, which the assert below then caught as if it were a kernel bug.
    let alloc_size = crate::mm::align_2m_checked(
        block_size.checked_add(dtv_size)?.checked_add(align)?,
    )?;
    let page_alloc = PageAlloc::new(alloc_size, crate::mm::pmm::Category::InitTls)?;
    let block = page_alloc.ptr();

    // Place TLS data near the end of the allocation (DTV at start, TLS after).
    // Align tls_start so that data_start (= block + tls_start) has tls_align alignment.
    let tls_start = (alloc_size - block_size) & !(align - 1);

    unsafe { core::ptr::write_bytes(block, 0, alloc_size); }

    for module in modules {
        if !module.is_static { continue; }
        if let Some(template) = &module.template {
            unsafe {
                core::ptr::copy_nonoverlapping(
                    template.base(),
                    block.add(tls_start + module.base_offset),
                    template.size(),
                );
            }
        }
    }

    // TP must be a user-visible physical address (mapped with USER bit in user page tables).
    let block_phys = DirectMap::from_ptr(block).phys();
    let tp_user = block_phys + (tls_start + total_memsz) as u64;
    let tp_kernel = block as u64 + (tls_start + total_memsz) as u64;
    unsafe { *(tp_kernel as *mut u64) = tp_user; }

    // Set up DTV at the start of the allocation.
    // DTV entries point to the start of each module's TLS data (user-visible addresses).
    //
    // The sizing above reserves `dtv_size + align` below the block and rounds
    // `tls_start` down by `align`, so this holds by construction and a failure
    // is a kernel bug, not a file.
    assert!(dtv_size < tls_start, "DTV overlaps TLS data");
    let dtv_kern = block as *mut u64;
    unsafe {
        *dtv_kern = 1;
        *dtv_kern.add(1) = DTV_INITIAL_CAPACITY as u64;
        for i in 0..DTV_INITIAL_CAPACITY {
            *dtv_kern.add(2 + i) = DTV_UNALLOCATED;
        }
        // Fill entries for static modules only: dtv[module_id - 1] = user addr of module's TLS data.
        // Dynamic modules (dlopen'd) stay DTV_UNALLOCATED — allocated on first access.
        for module in modules {
            if !module.is_static { continue; }
            let idx = module.module_id as usize;
            if idx > 0 && idx <= DTV_INITIAL_CAPACITY {
                let module_tls_addr = block_phys + (tls_start + module.base_offset) as u64;
                *dtv_kern.add(2 + idx - 1) = module_tls_addr;
            }
        }
    }

    // Write DTV pointer to TCB[8] (user-visible physical address of DTV)
    let dtv_user = block_phys;
    unsafe { *((tp_kernel + 8) as *mut u64) = dtv_user; }

    Some((page_alloc, tp_user))
}

/// Allocate a kernel stack and set up the initial register frame for context_switch.
/// Returns (alloc, saved_rsp).
pub(crate) fn alloc_kernel_stack(
    trampoline: unsafe extern "C" fn(),
    user_entry: u64,
    user_sp: u64,
    arg: u64,
) -> Option<(OwnedAlloc, u64)> {
    let alloc = OwnedAlloc::new(KERNEL_STACK_SIZE, 4096)?;
    scheduler::write_stack_canary(&alloc);
    let top = alloc.ptr() as u64 + KERNEL_STACK_SIZE as u64;
    // Must match context_switch layout: pushfq, push rbp..r15 (8 values) + return address
    let frame = (top - 8 * 8) as *mut u64;
    unsafe {
        *frame.add(0) = 0;                    // r15
        *frame.add(1) = arg;                  // r14
        *frame.add(2) = user_sp;              // r13
        *frame.add(3) = user_entry;           // r12
        *frame.add(4) = 0;                    // rbx
        *frame.add(5) = 0;                    // rbp
        *frame.add(6) = 0x002;                // RFLAGS (IF=0, AC=0)
        *frame.add(7) = trampoline as u64;    // return address
    }
    Some((alloc, frame as u64))
}


/// Entry point for new processes. Entered via context_switch's `ret`.
/// r12 = entry point, r13 = user stack pointer.
/// Releases the scheduler lock, then enters ring 3 via iretq.
#[unsafe(naked)]
pub(crate) extern "C" fn process_start() {
    naked_asm!(
        "push r12",
        "push r13",
        "call {unlock}",
        "pop r13",
        "pop r12",
        "push 0x1B",        // SS: user_data | RPL=3
        "push r13",         // RSP: user stack
        "push 0x202",       // RFLAGS: IF=1
        "push 0x23",        // CS: user_code | RPL=3
        "push r12",         // RIP: entry point
        "iretq",
        unlock = sym crate::sched::driver::trampoline_entry,
    );
}

/// Entry point for new threads. Entered via context_switch's `ret`.
/// r12 = entry point, r13 = user stack pointer, r14 = argument.
/// Releases the scheduler lock, then enters ring 3 via iretq with arg in rdi.
#[unsafe(naked)]
pub(crate) extern "C" fn thread_start() {
    naked_asm!(
        "push r12",
        "push r13",
        "push r14",
        "call {unlock}",
        "pop r14",
        "pop r13",
        "pop r12",
        "mov rdi, r14",
        "sub r13, 8",       // ABI: RSP must be 16n+8 at function entry
        "push 0x1B",        // SS: user_data | RPL=3
        "push r13",         // RSP: user stack
        "push 0x202",       // RFLAGS: IF=1
        "push 0x23",        // CS: user_code | RPL=3
        "push r12",         // RIP: entry point
        "iretq",
        unlock = sym crate::sched::driver::trampoline_entry,
    );
}

fn make_name(path: &str) -> [u8; 28] {
    let filename = path.rsplit('/').next().unwrap_or(path);
    let mut name = [0u8; 28];
    let len = filename.len().min(27);
    name[..len].copy_from_slice(&filename.as_bytes()[..len]);
    name
}

/// Build a child's FdTable from (child_fd, parent_fd) pairs.
/// Duplicates each referenced parent descriptor into the child table.
pub fn build_child_fds(pairs: &[[u32; 2]]) -> Result<FdTable, SyscallError> {
    let data_arc = fd_owner_data();
    let data = data_arc.lock();
    let mut fds = FdTable::new();
    for &[child_fd, parent_fd] in pairs {
        if let Some(desc) = data.fds.get(parent_fd) {
            let cloned = desc.clone();
            fds.insert_at(child_fd, cloned)?;
        }
    }
    Ok(fds)
}

/// User virtual address space starts at 1TB — well above any direct-mapped physical RAM.
const USER_VM_BASE: u64 = 0x100_0000_0000;

/// Convert an ELF virtual address to a file offset by searching PT_LOAD segments.
/// Falls back to extrapolating from the nearest segment below `vaddr`, which is
/// what `.rela.dyn` and friends need when the linker places them outside any
/// PT_LOAD.
///
/// `None` when there is no segment at or below `vaddr` to extrapolate from, or
/// when the extrapolation overflows. Every `vaddr` here is a `DT_*` tag or a
/// program-header field, so it is untrusted: the answer to "this address is in
/// no segment" is that the binary is malformed, not that the kernel dies.
fn vaddr_to_file_offset(segments: &[elf::ElfSegment], vaddr: u64) -> Option<u64> {
    for seg in segments {
        if vaddr >= seg.vaddr && vaddr < seg.vaddr + seg.filesz {
            return seg.file_offset.checked_add(vaddr - seg.vaddr);
        }
    }
    let mut best: Option<&elf::ElfSegment> = None;
    for seg in segments {
        if seg.vaddr <= vaddr && best.map_or(true, |b| seg.vaddr > b.vaddr) {
            best = Some(seg);
        }
    }
    let seg = best?;
    seg.file_offset.checked_add(vaddr - seg.vaddr)
}

/// Read a byte range from a file using its block map via the page cache.
///
/// Returns only the part of the request the file actually holds. Every `len`
/// here comes off an ELF — `DT_STRSZ`, a symbol count, `e_shnum * e_shentsize`
/// — so an unclamped `Vec::with_capacity` is a heap allocation sized by
/// untrusted input, and past EOF there is nothing to read anyway (the
/// backings zero-fill). Callers already treat a short return as
/// "table truncated, stop"; they all length-check before indexing.
pub(crate) fn read_file_range(backing: &dyn crate::file_backing::FileBacking, offset: u64, len: usize) -> Vec<u8> {
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
        // answer this function's callers already handle: every one of them
        // length-checks before indexing and treats a short return as a
        // truncated table. Zeros would instead be a table full of null
        // entries, which is a different and much quieter kind of wrong.
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
/// `None` above [`crate::mm::MAX_HEAP_ALLOC`], which is the point where the `Vec`
/// stops being an allocation failure and becomes an assert in the kernel
/// heap's page source. Refusing is deliberate: clamping the length instead
/// would load the binary with a table that is short and nothing downstream
/// could tell.
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

/// Resolve a single exe TPOFF relocation entry to a pre-computed i64 value.
/// Handles both r_sym == 0 (simple offset) and r_sym != 0 (cross-library lookup).
///
/// `symtab_file_off` is `None` when `DT_SYMTAB` names no file offset at all,
/// which is the same position as a symbol read that comes back short: there is
/// no symbol to resolve against, so the unnamed form is the only answer left.
fn resolve_exe_tpoff(
    r_sym: u32,
    r_addend: i64,
    exe_base_offset: usize,
    total_memsz: usize,
    symtab_file_off: Option<u64>,
    backing: &dyn crate::file_backing::FileBacking,
    dynstr_data: &[u8],
    tls_info: &elf::TlsModuleInfo,
) -> i64 {
    let (Some(symtab_file_off), true) = (symtab_file_off, r_sym != 0) else {
        return exe_base_offset as i64 + r_addend - total_memsz as i64;
    };

    let Some(sym_off) = symtab_file_off.checked_add(r_sym as u64 * elf::SYM_SIZE as u64) else {
        return exe_base_offset as i64 + r_addend - total_memsz as i64;
    };
    let sym_data = read_file_range(backing, sym_off, elf::SYM_SIZE);
    if sym_data.len() < elf::SYM_SIZE {
        return exe_base_offset as i64 + r_addend - total_memsz as i64;
    }
    let sym = elf::read_sym(&sym_data, 0);

    if sym.st_shndx != 0 {
        exe_base_offset as i64 + sym.st_value as i64 + r_addend - total_memsz as i64
    } else {
        let sym_name = elf::sym_name(&sym, dynstr_data);

        for lib in tls_info.libs {
            if lib.tls_memsz == 0 { continue; }
            if let Some(sym_tls_offset) = elf::tls_dlsym_pub(lib, sym_name) {
                let other_base_offset = tls_info.modules.iter()
                    .find(|m| m.template == lib.tls_template)
                    .map(|m| m.base_offset)
                    .unwrap_or(0);
                return other_base_offset as i64 + sym_tls_offset as i64 - total_memsz as i64;
            }
        }
        log!("tpoff: unresolved exe TLS symbol: {}", sym_name);
        0
    }
}

/// Insert demand-paged regions for each PT_LOAD segment into the address space.
///
/// `base + seg.vaddr` must not overflow, which is what `image_fits_user_half`
/// establishes before this is called.
///
/// Returns `Err` when two segments would claim the same page. A segment's
/// regions are page-rounded, so segments that merely *share* a page are two
/// VMAs at one address and `insert_region` asserts — a kernel-bug assert
/// reached from a file. Checked over every pair before the first insert, so a
/// refusal leaves the address space as it found it.
fn insert_elf_regions(
    addr_space: &mut crate::mm::paging::AddressSpace,
    layout: &elf::ElfLayout,
    base: u64,
    backing: &Arc<dyn crate::file_backing::FileBacking>,
) -> Result<(), SyscallError> {
    use crate::vma::{Region, RegionKind};

    let extent = |seg: &elf::ElfSegment| {
        ((base + seg.vaddr) & !0xFFF, (base + seg.vaddr + seg.memsz + 0xFFF) & !0xFFF)
    };
    for (i, a) in layout.segments.iter().enumerate() {
        let (a_start, a_end) = extent(a);
        for b in &layout.segments[i + 1..] {
            let (b_start, b_end) = extent(b);
            if a_start < b_end && b_start < a_end {
                log!("spawn: PT_LOAD pages [{:#x},{:#x}) and [{:#x},{:#x}) overlap",
                    a_start, a_end, b_start, b_end);
                return Err(SyscallError::InvalidArgument);
            }
        }
    }

    for seg in &layout.segments {
        let (seg_start, seg_end) = extent(seg);
        // A segment that covers no page maps nothing, and a zero-size region
        // would sit in the map where `find_region` cannot see past it.
        if seg_end == seg_start { continue; }

        let file_block_start = seg.file_offset / 4096;
        let file_blocks_needed = ((seg.filesz + (seg.file_offset % 4096) + 4095) / 4096) as usize;
        let file_backed_end = seg_start + file_blocks_needed as u64 * 4096;

        if file_blocks_needed > 0 && file_backed_end > seg_start {
            addr_space.insert_region(UserAddr::new(seg_start), Region {
                size: file_backed_end.min(seg_end) - seg_start,
                writable: seg.writable,
                kind: RegionKind::FileBacked {
                    backing: Arc::clone(backing),
                    file_offset: file_block_start * 4096,
                    file_size: seg.filesz + (seg.file_offset % 4096),
                },
            });
        }

        if file_backed_end < seg_end {
            let anon_start = file_backed_end.max(seg_start);
            addr_space.insert_region(UserAddr::new(anon_start), Region {
                size: seg_end - anon_start,
                writable: seg.writable,
                kind: RegionKind::Anonymous,
            });
        }
    }
    Ok(())
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
/// The image occupies `[USER_VM_BASE, USER_VM_BASE + vaddr_max - vaddr_min)`
/// because `vaddr_min` is the smallest `p_vaddr` and `vaddr_max` the largest
/// `p_vaddr + p_memsz`, so one range check covers every segment. The bound is
/// `user_span::in_user_half`'s, i.e. the hardware's user/kernel
/// split, not a policy number.
fn image_fits_user_half(layout: &elf::ElfLayout) -> bool {
    let span = layout.vaddr_max - layout.vaddr_min;
    crate::mm::user_span::in_user_half(USER_VM_BASE, span)
}

/// Build TLS module layout from loaded shared libraries and the exe's TLS segment.
fn build_tls_layout(
    loaded_libs: &[elf::LoadedLib],
    layout: &elf::ElfLayout,
    exe_tls_template: Option<&OwnedAlloc>,
) -> (Vec<elf::TlsModule>, usize, usize, u64) {
    let mut modules = Vec::new();
    let mut cursor = 0usize;
    let mut max_align = 1usize;
    // Module ID 1 = exe, 2+ = shared libs. Libs are laid out first in the block,
    // then the exe. Module IDs are assigned in layout order (libs first).
    let mut next_module_id = 2u64; // 1 reserved for exe

    for lib in loaded_libs {
        if lib.tls_memsz > 0 {
            if cursor > 0 { cursor = (cursor + 15) & !15; }
            let mid = next_module_id;
            next_module_id += 1;
            modules.push(elf::TlsModule {
                template: lib.tls_template,
                memsz: lib.tls_memsz, base_offset: cursor, module_id: mid,
                is_static: true,
            });
            cursor += lib.tls_memsz;
            if lib.tls_align > max_align { max_align = lib.tls_align; }
        }
    }

    if layout.tls_memsz > 0 {
        if cursor > 0 { cursor = (cursor + 15) & !15; }
        let template = exe_tls_template.map(|buf| buf.slice(layout.tls_filesz));
        modules.push(elf::TlsModule {
            template,
            memsz: layout.tls_memsz, base_offset: cursor, module_id: 1,
            is_static: true,
        });
        cursor += layout.tls_memsz;
        if layout.tls_align > max_align { max_align = layout.tls_align; }
    }

    (modules, cursor, max_align, next_module_id)
}

pub fn spawn(argv: &[&str], fds: FdTable, parent: Option<Pid>, env: Vec<u8>) -> Result<Pid, SyscallError> {
    // An argv of only separators survives the split in sys_spawn as an empty
    // slice; there is no argv[0] to load.
    let Some(&path) = argv.first() else {
        return Err(SyscallError::InvalidArgument);
    };
    let t0 = crate::clock::nanos_since_boot();

    // 1. Open file backing from VFS (follows symlinks)
    let backing: Arc<dyn crate::file_backing::FileBacking> = match vfs::lock().open_backing(path) {
        Some(b) => b,
        None => {
            log!("spawn: {}: not found", path);
            return Err(SyscallError::NotFound);
        }
    };

    let header_size = 4096.min(backing.file_size() as usize);
    let header_data = read_file_range(backing.as_ref(), 0, header_size);

    let layout = match elf::parse_layout(&header_data) {
        Ok(l) => l,
        Err(msg) => {
            log!("spawn: {}: {}", path, msg);
            return Err(SyscallError::InvalidArgument);
        }
    };

    // 3b. Parse PT_DYNAMIC from block map (not available in the header buffer)
    let dyn_info = if let Some((dyn_off, _, dyn_size)) = layout.dynamic {
        let dyn_data = read_elf_table(backing.as_ref(), dyn_off, dyn_size as usize)
            .ok_or(SyscallError::ResourceExhausted)?;
        elf::parse_dynamic(&dyn_data)
    } else {
        elf::DynamicInfo::empty()
    };

    let t1 = crate::clock::nanos_since_boot();

    if !image_fits_user_half(&layout) {
        log!("spawn: {}: image spans {:#x} bytes, past the user half from {:#x}",
            path, layout.vaddr_max - layout.vaddr_min, USER_VM_BASE);
        return Err(SyscallError::InvalidArgument);
    }
    let base = USER_VM_BASE - layout.vaddr_min;

    // Every `DT_*` tag below is a file-supplied vaddr the loader has to turn
    // into a file offset, and there is no answer for one that lies below every
    // PT_LOAD. Refusing the binary is the answer; the alternative this replaced
    // was a panic in syscall context.
    let file_off = |what: &str, vaddr: u64| match vaddr_to_file_offset(&layout.segments, vaddr) {
        Some(off) => Ok(off),
        None => {
            log!("spawn: {}: {} vaddr {:#x} is in or near no PT_LOAD segment", path, what, vaddr);
            Err(SyscallError::InvalidArgument)
        }
    };

    // And every table length below is a number the file names. Reading one
    // that exceeds a single kernel allocation is a panic in the heap's page
    // source, so it is a refusal here.
    let table = |what: &str, off: u64, len: usize| match read_elf_table(backing.as_ref(), off, len) {
        Some(v) => Ok(v),
        None => {
            log!("spawn: {}: {} declares {} bytes, past one kernel allocation", path, what, len);
            Err(SyscallError::ResourceExhausted)
        }
    };

    let rela_data = if dyn_info.rela_size > 0 {
        let rela_file_off = file_off("DT_RELA", dyn_info.rela_vaddr)?;
        table("DT_RELASZ", rela_file_off, dyn_info.rela_size as usize)?
    } else if layout.dynamic.is_none() {
        // No PT_DYNAMIC — fall back to finding .rela.dyn from section headers
        if let Some((shoff, shnum, shentsize)) = layout.section_headers {
            let shdr_data = table("e_shnum", shoff, shnum as usize * shentsize as usize)?;
            let bk = backing.as_ref();
            if let Some((rela_off, rela_size)) = elf::find_rela_dyn_from_sections(
                &shdr_data, shentsize, &|off, len| read_file_range(bk, off, len),
            ) {
                table("SHT_RELA sh_size", rela_off, rela_size as usize)?
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };
    let jmprel_data = if dyn_info.jmprel_size > 0 {
        let jmprel_file_off = file_off("DT_JMPREL", dyn_info.jmprel_vaddr)?;
        table("DT_PLTRELSZ", jmprel_file_off, dyn_info.jmprel_size as usize)?
    } else {
        Vec::new()
    };
    let Some(parsed_relas) = elf::parse_rela_entries(&rela_data, &jmprel_data) else {
        log!("spawn: {}: relocation tables do not fit one allocation", path);
        return Err(SyscallError::ResourceExhausted);
    };

    // Reserved from the counts rather than grown: `add_u64` is called for
    // every RELATIVE and TPOFF64 entry and for each GLOB_DAT that resolves, so
    // these are exact upper bounds and nothing here reallocates.
    let u64_writes = parsed_relas.relative.len()
        + parsed_relas.glob_dat.len()
        + parsed_relas.tpoff64.len();
    let Some(mut reloc_index) =
        elf::RelocationIndex::with_capacity(u64_writes, parsed_relas.tpoff32.len())
    else {
        log!("spawn: {}: {} relocations do not fit one index", path, u64_writes);
        return Err(SyscallError::ResourceExhausted);
    };
    for &(r_offset, r_addend) in &parsed_relas.relative {
        reloc_index.add_u64(r_offset, (base as i64 + r_addend) as u64);
    }

    let t2 = crate::clock::nanos_since_boot();

    // 7. Load shared libraries from block map (no full binary read)
    // Read DT_STRTAB from block map to get library names
    let (mut loaded_libs, lib_paths) = if !dyn_info.needed_strtab_offsets.is_empty() && dyn_info.strsz > 0 {
        let strtab_file_off = file_off("DT_STRTAB", dyn_info.strtab_vaddr)?;
        let strtab_data = table("DT_STRSZ", strtab_file_off, dyn_info.strsz as usize)?;

        let exe_dir = path.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
        let mut libs = Vec::new();
        let mut lib_paths_vec = Vec::new();

        for &name_offset in &dyn_info.needed_strtab_offsets {
            let name_off = name_offset as usize;
            if name_off >= strtab_data.len() { continue; }
            let name_end = strtab_data[name_off..].iter().position(|&b| b == 0)
                .unwrap_or(strtab_data.len() - name_off);
            let lib_name = core::str::from_utf8(&strtab_data[name_off..name_off + name_end]).unwrap_or("");
            if lib_name.is_empty() { continue; }

            let lib_path = alloc::format!("{}/{}", exe_dir, lib_name);
            let t_load0 = crate::clock::nanos_since_boot();

            if let Some(lib) = elf::try_clone_cached(&lib_path) {
                lib_paths_vec.push(lib_path);
                libs.push(lib);
                continue;
            }

            let so_backing = {
                let b = vfs::lock().open_backing(&lib_path);
                match b {
                    Some(b) => b,
                    None => {
                        let fallback = alloc::format!("/lib/{}", lib_name);
                        match vfs::lock().open_backing(&fallback) {
                            Some(b) => b,
                            None => {
                                log!("spawn: {}: failed to load {}: not found", path, lib_name);
                                return Err(SyscallError::NotFound);
                            }
                        }
                    }
                }
            };

            match elf::load_shared_lib(so_backing.as_ref()) {
                Ok((lib, rw_offset, rw_size)) => {
                    let t_load1 = crate::clock::nanos_since_boot();
                    log!("dynamic: loaded {} base={:#x} ({} syms, {}ms)",
                        lib_name, lib.phys_base, lib.sym_count,
                        (t_load1 - t_load0) / 1_000_000);
                    let lib = elf::cache_loaded_lib_pub(&lib_path, lib, rw_offset, rw_size);
                    lib_paths_vec.push(lib_path);
                    libs.push(lib);
                }
                Err(e) => {
                    log!("spawn: {}: failed to load {}: {}", path, lib_name, e);
                    return Err(SyscallError::NotFound);
                }
            }
        }

        if !libs.is_empty() {
            let dynstr_file_off = file_off("DT_STRTAB", dyn_info.strtab_vaddr)?;
            let dynstr_data = table("DT_STRSZ", dynstr_file_off, dyn_info.strsz as usize)?;

            // Determine .dynsym entry count via GNU hash table or SYMTAB/STRTAB gap
            let sym_count = if dyn_info.gnu_hash_vaddr != 0 {
                let gnu_hash_file_off = file_off("DT_GNU_HASH", dyn_info.gnu_hash_vaddr)?;
                // Read enough for the hash table (header + bloom + buckets + chains)
                // Start with a generous read; typical .dynsym for executables is small
                let gnu_hash_data = read_file_range(backing.as_ref(), gnu_hash_file_off,
                    64 * 1024); // 64KB should cover most exe gnu_hash tables
                elf::gnu_hash_sym_count_from_data(&gnu_hash_data)
            } else if dyn_info.symtab_vaddr != 0 && dyn_info.strtab_vaddr > dyn_info.symtab_vaddr {
                // No GNU hash: infer from SYMTAB-to-STRTAB gap (24 bytes per entry)
                ((dyn_info.strtab_vaddr - dyn_info.symtab_vaddr) / 24) as usize
            } else {
                0
            };

            let mut exe_sym_map = if sym_count > 0 {
                let symtab_file_off = file_off("DT_SYMTAB", dyn_info.symtab_vaddr)?;
                let dynsym_data = table("symbol count", symtab_file_off, sym_count * elf::SYM_SIZE)?;
                elf::build_exe_sym_map(&dynsym_data, &dynstr_data, sym_count, UserAddr::new(base))
            } else {
                hashbrown::HashMap::new()
            };

            // If .dynsym has no defined symbols, fall back to .symtab from section headers.
            // This handles PIE executables that don't export symbols via --export-dynamic.
            if exe_sym_map.is_empty() {
                if let Some((shoff, shnum, shentsize)) = layout.section_headers {
                    let shdr_data = table("e_shnum", shoff, shnum as usize * shentsize as usize)?;
                    if let Some(m) = elf::build_symtab_map(&shdr_data, shentsize, backing.as_ref(), UserAddr::new(base)) {
                        exe_sym_map = m;
                    }
                }
            }

            let t_syms = crate::clock::nanos_since_boot();
            log!("dynamic: {} exe syms hashed from block map in {}ms",
                exe_sym_map.len(), (t_syms - t2) / 1_000_000);

            // NOTE: lib bind relocs and exe GLOB_DAT are deferred until after
            // VMA addresses are assigned (user_base must be correct for GOT values).
        }

        (libs, lib_paths_vec)
    } else {
        (Vec::new(), Vec::new())
    };

    let t_deps = crate::clock::nanos_since_boot();

    // 8. Create user address space (PML4) — ELF segments are demand-faulted
    let child_pt: PageTables = Arc::new(Lock::new(crate::mm::paging::AddressSpace::new_user()));

    // 8a. Insert ELF regions into the child address space (demand-paged)
    insert_elf_regions(&mut child_pt.lock(), &layout, base, &backing)?;

    // 8b. Map shared libraries and assign virtual addresses.
    // This MUST happen BEFORE relocation processing so that user_base is correct
    // when GOT entries are written (RELATIVE: user_base + addend, GLOB_DAT: user_base + sym.st_value).
    for lib in &mut loaded_libs {
        match &lib.memory {
            elf::LibMemory::Owned(alloc) => {
                let phys = DirectMap::phys_of(alloc.ptr());
                let Some((vaddr, _)) = vma_map(&child_pt, phys, alloc.size() as u64) else {
                    log!("spawn: {}: out of virtual address space for a library", path);
                    return Err(SyscallError::ResourceExhausted);
                };
                let delta = vaddr.raw() as i64 - lib.user_base.raw() as i64;
                lib.user_base = vaddr;
                lib.user_end = (lib.user_end as i64 + delta) as u64;
            }
            elf::LibMemory::Shared { rw_alloc, cached_image, rw_offset, .. } => {
                let cached_phys = cached_image.phys();
                let Some((lib_vaddr, _)) = vma_map(&child_pt, cached_phys, cached_image.size() as u64) else {
                    log!("spawn: {}: out of virtual address space for a library", path);
                    return Err(SyscallError::ResourceExhausted);
                };
                let num_rw_pages = rw_alloc.size() / PAGE_2M as usize;
                let rw_phys = DirectMap::phys_of(rw_alloc.ptr());
                for i in 0..num_rw_pages {
                    let user_virt = lib_vaddr.raw() + *rw_offset as u64 + i as u64 * PAGE_2M;
                    let phys = rw_phys + i as u64 * PAGE_2M;
                    child_pt.lock().remap(UserAddr::new(user_virt), phys, true);
                }
                let delta = lib_vaddr.raw() as i64 - lib.user_base.raw() as i64;
                lib.user_base = lib_vaddr;
                lib.user_end = (lib.user_end as i64 + delta) as u64;
            }
        }
    }

    // 8b. Rebase RELATIVE relocations: load_shared_lib applied them with phys_base,
    // but now user_base differs. Add delta = (user_base - phys_base) to each entry.
    for lib in &loaded_libs {
        let delta = lib.user_base.raw() as i64 - lib.phys_base as i64;
        if delta != 0 {
            elf::rebase_relative_relocs(lib, delta);
        }
    }

    // 8c. NOW process library bind relocations (user_base is correct for all libs).
    if !loaded_libs.is_empty() {
        let dynstr_data = if dyn_info.strsz > 0 {
            let dynstr_file_off = file_off("DT_STRTAB", dyn_info.strtab_vaddr)?;
            table("DT_STRSZ", dynstr_file_off, dyn_info.strsz as usize)?
        } else {
            Vec::new()
        };

        let sym_count = if dyn_info.gnu_hash_vaddr != 0 {
            let gnu_hash_file_off = file_off("DT_GNU_HASH", dyn_info.gnu_hash_vaddr)?;
            let gnu_hash_data = read_file_range(backing.as_ref(), gnu_hash_file_off, 64 * 1024);
            elf::gnu_hash_sym_count_from_data(&gnu_hash_data)
        } else if dyn_info.symtab_vaddr != 0 && dyn_info.strtab_vaddr > dyn_info.symtab_vaddr {
            ((dyn_info.strtab_vaddr - dyn_info.symtab_vaddr) / 24) as usize
        } else {
            0
        };

        let symtab_file_off = if dyn_info.symtab_vaddr != 0 {
            Some(file_off("DT_SYMTAB", dyn_info.symtab_vaddr)?)
        } else {
            None
        };

        let exe_sym_map = match (sym_count, symtab_file_off) {
            (0, _) | (_, None) => hashbrown::HashMap::new(),
            (_, Some(off)) => {
                let Ok(dynsym_data) = table("symbol count", off, sym_count * elf::SYM_SIZE) else {
                    return Err(SyscallError::ResourceExhausted);
                };
                elf::build_exe_sym_map(&dynsym_data, &dynstr_data, sym_count, UserAddr::new(base))
            }
        };

        for lib in &loaded_libs {
            elf::resolve_lib_bind_relocs_pub(lib, &exe_sym_map, &loaded_libs);
        }

        for &(r_offset, r_sym, _r_addend) in &parsed_relas.glob_dat {
            if r_sym == 0 { continue; }
            let Some(sym_off) = symtab_file_off
                .and_then(|off| off.checked_add(r_sym as u64 * elf::SYM_SIZE as u64))
            else {
                continue;
            };
            let sym_data = read_file_range(backing.as_ref(), sym_off, elf::SYM_SIZE);
            if sym_data.len() < elf::SYM_SIZE { continue; }
            let sym = elf::read_sym(&sym_data, 0);
            let sym_name = elf::sym_name(&sym, &dynstr_data);
            let resolved = loaded_libs.iter().find_map(|lib| elf::gnu_dlsym_pub(lib, sym_name));
            match resolved {
                Some(addr) => reloc_index.add_u64(r_offset, addr.raw()),
                None => log!("dynamic: unresolved exe symbol: {}", sym_name),
            }
        }
    }

    // 9. Stack at fixed virtual address (STACK_BASE from vma.rs)
    let stack_pages = match PageAlloc::new(USER_STACK_SIZE, crate::mm::pmm::Category::Stack) {
        Some(a) => a,
        None => {
            log!("spawn: {}: failed to allocate user stack ({} bytes)", path, USER_STACK_SIZE);
            return Err(SyscallError::ResourceExhausted);
        }
    };
    let stack_phys = DirectMap::from_phys(stack_pages.phys());
    let stack_vaddr = UserAddr::new(crate::vma::STACK_BASE);
    let user_stack = UserStack::new(stack_vaddr, stack_phys, USER_STACK_SIZE as u64);
    {
        let mut pt = child_pt.lock();
        pt.map_range(stack_vaddr, stack_pages.phys(), USER_STACK_SIZE as u64, true, CachePolicy::DeferToMtrr);
        pt.insert_region(stack_vaddr, crate::vma::Region {
            size: USER_STACK_SIZE as u64,
            writable: true,
            kind: crate::vma::RegionKind::Anonymous,
        });
    }

    let exe_tls_template = if layout.tls_memsz > 0 {
        let tls_file_off = file_off("PT_TLS", layout.tls_vaddr)?;
        // Read straight into the `tls_memsz`-sized buffer rather than via an
        // intermediate `Vec` sized by the file's `tls_filesz`. `OwnedAlloc`
        // zeroes, so the `.tbss` tail needs no second pass.
        //
        // A `tls_memsz` above what one heap allocation can hold is refused by
        // `OwnedAlloc::new` itself, so there is no second copy of that ceiling
        // here.
        let Some(tls_buf) = OwnedAlloc::new(layout.tls_memsz, 16) else {
            log!("spawn: {}: cannot allocate a {}-byte TLS template", path, layout.tls_memsz);
            return Err(SyscallError::ResourceExhausted);
        };
        if elf::read_backing_into(backing.as_ref(), tls_file_off, tls_buf.ptr(), layout.tls_filesz)
            .is_err()
        {
            log!("spawn: {}: the TLS template could not be read off the device", path);
            return Err(SyscallError::NotFound);
        }
        Some(tls_buf)
    } else {
        None
    };

    let (tls_modules, tls_total_memsz, max_tls_align, next_tls_module_id) =
        build_tls_layout(&loaded_libs, &layout, exe_tls_template.as_ref());

    // Apply TLS relocations for shared libraries loaded at startup.
    let tls_info = elf::TlsModuleInfo { libs: &loaded_libs, modules: &tls_modules };
    for lib in &loaded_libs {
        // Match by template pointer — unique per lib since each points into a distinct ELF mapping.
        // Libs without TLS (tls_memsz=0) have null template and won't match any module.
        let module = tls_modules.iter().find(|m| m.template == lib.tls_template);
        let lib_base_offset = module.map(|m| m.base_offset).unwrap_or(0);
        // IE model: TPOFF refs to static-block TLS (static modules and cross-module refs)
        elf::apply_tpoff_relocs(lib, lib_base_offset, tls_total_memsz, &tls_info);
        // GD model: DTPMOD64/DTPOFF64 for this lib's own TLS (DTV-based dynamic access)
        if let Some(m) = module {
            elf::apply_dtpmod_relocs(lib, m.module_id, &tls_info);
        }
    }
    // Resolve exe TPOFF relocations → add pre-computed values to reloc index
    {
        let exe_base_offset = tls_modules.iter()
            .find(|m| m.module_id == 1)
            .map(|m| m.base_offset)
            .unwrap_or(0);

        let dynstr_data = if dyn_info.strsz > 0 {
            let dynstr_file_off = file_off("DT_STRTAB", dyn_info.strtab_vaddr)?;
            table("DT_STRSZ", dynstr_file_off, dyn_info.strsz as usize)?
        } else {
            Vec::new()
        };
        let symtab_file_off = if dyn_info.symtab_vaddr != 0 {
            Some(file_off("DT_SYMTAB", dyn_info.symtab_vaddr)?)
        } else {
            None
        };

        for &(r_offset, r_sym, r_addend) in &parsed_relas.tpoff64 {
            let tpoff = resolve_exe_tpoff(
                r_sym, r_addend, exe_base_offset, tls_total_memsz,
                symtab_file_off, backing.as_ref(), &dynstr_data, &tls_info,
            );
            reloc_index.add_u64(r_offset, tpoff as u64);
        }
        for &(r_offset, r_sym, r_addend) in &parsed_relas.tpoff32 {
            let tpoff = resolve_exe_tpoff(
                r_sym, r_addend, exe_base_offset, tls_total_memsz,
                symtab_file_off, backing.as_ref(), &dynstr_data, &tls_info,
            );
            reloc_index.add_i32(r_offset, tpoff as i32);
        }
    }

    reloc_index.finalize();
    let reloc_index = if reloc_index.len() > 0 {
        log!("ELF: {} relocations indexed (RELATIVE + GLOB_DAT + TPOFF)", reloc_index.len());
        Some(Arc::new(reloc_index))
    } else {
        None
    };

    let (tls_template, tls_memsz) = if !tls_modules.is_empty() {
        (tls_modules[0].template, tls_modules[0].memsz)
    } else {
        (None, 0)
    };

    log!("spawn: TLS {} modules, total_memsz={}", tls_modules.len(), tls_total_memsz);
    let (tls_alloc, fs_base) = if tls_total_memsz > 0 {
        match setup_combined_tls(&tls_modules, tls_total_memsz, max_tls_align) {
            Some(v) => v,
            None => {
                log!("spawn: {}: failed to allocate TLS ({} bytes)", path, tls_total_memsz);
                return Err(SyscallError::ResourceExhausted);
            }
        }
    } else {
        match setup_tls(None, 0, 1) {
            Some(v) => v,
            None => {
                log!("spawn: {}: failed to allocate TLS (empty)", path);
                return Err(SyscallError::ResourceExhausted);
            }
        }
    };
    // TLS mapped via address space — rebase all user-visible pointers from phys to vaddr
    let tls_phys = tls_alloc.phys();
    let Some((tls_vaddr, _)) = vma_map(&child_pt, tls_phys, tls_alloc.size() as u64) else {
        log!("spawn: {}: out of virtual address space for TLS", path);
        return Err(SyscallError::ResourceExhausted);
    };
    let tls_rebase = tls_vaddr.raw() as i64 - tls_phys as i64;
    let fs_base = (fs_base as i64 + tls_rebase) as u64;
    unsafe {
        let tls_base_ptr = DirectMap::from_phys(tls_phys).as_mut_ptr::<u8>();
        let tp_kern = tls_base_ptr.add((fs_base - tls_vaddr.raw()) as usize);
        let self_ptr = tp_kern as *mut u64;
        *self_ptr = fs_base;
        let dtv_phys = *self_ptr.add(1);
        *self_ptr.add(1) = (dtv_phys as i64 + tls_rebase) as u64;
        let dtv_kern = tls_base_ptr as *mut u64;
        let dtv_len = *dtv_kern.add(1) as usize;
        for i in 0..dtv_len {
            let entry = *dtv_kern.add(2 + i);
            if entry != !0u64 && entry != 0 {
                *dtv_kern.add(2 + i) = (entry as i64 + tls_rebase) as u64;
            }
        }
    }

    let entry = base + layout.entry_vaddr;
    let sp = user_stack.write_argv(argv);

    let t_tls = crate::clock::nanos_since_boot();

    let syms = if let Some((sh_off, sh_num, sh_entsize)) = layout.section_headers {
        crate::process::find_symtab_in_memory(
            backing.as_ref(), sh_off, sh_num as usize, sh_entsize as usize,
            base,
            base + layout.vaddr_min, base + layout.vaddr_max,
            user_stack.base().raw(), user_stack.top(),
        )
    } else {
        SymbolTable::empty_with_bounds(
            base + layout.vaddr_min, base + layout.vaddr_max,
            user_stack.base().raw(), user_stack.top(),
        )
    };

    let (ks_alloc, ks_rsp) = match alloc_kernel_stack(process_start, entry, sp, 0) {
        Some(ks) => ks,
        None => {
            log!("spawn: {}: failed to allocate kernel stack", path);
            return Err(SyscallError::ResourceExhausted);
        }
    };


    let cwd = match parent {
        Some(ppid) => {
            let arc = {
                let guard = PROCESS_TABLE.lock();
                let table = guard.as_ref().unwrap();
                Arc::clone(table.get(ppid).unwrap().process_data())
            };
            let cwd = arc.lock().cwd.clone();
            cwd
        }
        None => String::from("/"),
    };

    let proc_data = Arc::new(Lock::new(ProcessData {
        fds,
        cwd,
        env,
        elf: ElfInfo {
            elf_alloc: exe_tls_template, // TLS template allocation (if any)
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
            exe_vaddr_max: base + layout.vaddr_max - layout.vaddr_min,
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
        child_stats: Vec::new(),
    }));

    let thread_data = Arc::new(Lock::new(ThreadData {
        tls_pages: Some(crate::process::MappedPages::new(tls_vaddr, tls_alloc)),
        stack_pages: Some(stack_pages),
        user_stack_base: user_stack.base(),
        user_stack_size: user_stack.size(),
        syscall_counts: [0; 64],
        syscall_total: 0,
        syscall_total_ns: 0,
    }));

    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().unwrap();
    let pid = table.insert_with(|pid| ProcessEntry::new(
        pid,
        parent,
        make_name(path),
        proc_data,
        Arc::new(Lock::new(syms)),
        ThreadEntry::new(thread_data),
    ));
    let tid = table.get(pid).unwrap().main_tid();

    // Placed while still holding the table lock: kill_process claims teardown
    // under this lock, so once the pid is visible its main thread is already in
    // the scheduler — a retire sweep can never miss it in a table-insert→place
    // gap.
    let sched = scheduler::enqueue_new(
        scheduler::TaskId(pid, tid),
        ks_alloc,
        ks_rsp,
        Some(child_pt.clone()),
        fs_base,
    );
    table.get_mut(pid).unwrap().threads_mut().get_mut(tid).unwrap().set_sched(sched);
    drop(guard);

    let t3 = crate::clock::nanos_since_boot();
    log!("spawn: {} pid={} tid={} base={:#x} entry={:#x} cr3={:#x} (layout={}ms relocs={}ms deps={}ms tls={}ms total={}ms)",
        path, pid, tid, base, entry, child_pt.lock().cr3().phys(),
        (t1 - t0) / 1_000_000, (t2 - t1) / 1_000_000, (t_deps - t2) / 1_000_000,
        (t_tls - t_deps) / 1_000_000, (t3 - t0) / 1_000_000);

    Ok(pid)
}

/// Spawn a process from kernel context (during boot). Resolves bare names
/// to `/bin/<name>`. Panics on failure.
pub fn spawn_kernel(argv: &[&str]) -> Pid {
    let mut fds = FdTable::new();
    for fd in 0..3 {
        fds.insert_at(fd, Descriptor::SerialConsole)
            .expect("spawn_kernel: three fds cannot exhaust an empty table");
    }
    spawn(argv, fds, None, Vec::new()).expect("spawn_kernel: failed to spawn")
}
