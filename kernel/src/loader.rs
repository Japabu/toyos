use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::arch::naked_asm;
use crate::elf;
use crate::fd::{self, Descriptor, FdTable};
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

// ---------------------------------------------------------------------------
// TLS constants
// ---------------------------------------------------------------------------

const TCB_SIZE: usize = 64;
/// Initial DTV capacity (number of module entries).
const DTV_INITIAL_CAPACITY: usize = 64;
/// Header size: generation (8) + len (8).
const DTV_HEADER_SIZE: usize = 16;
/// Sentinel value for unallocated DTV entries.
const DTV_UNALLOCATED: u64 = !0u64;

// ---------------------------------------------------------------------------
// TLS setup
// ---------------------------------------------------------------------------

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
pub fn setup_combined_tls(
    modules: &[crate::elf::TlsModule],
    total_memsz: usize,
    tls_align: usize,
) -> Option<(PageAlloc, u64)> {
    let block_size = total_memsz + TCB_SIZE;
    let alloc_size = crate::mm::align_2m(block_size + tls_align);
    let page_alloc = PageAlloc::new(alloc_size, crate::mm::pmm::Category::InitTls)?;
    let block = page_alloc.ptr();

    // Place TLS data near the end of the allocation (DTV at start, TLS after).
    // Align tls_start so that data_start (= block + tls_start) has tls_align alignment.
    let align = if tls_align > 1 { tls_align } else { 8 };
    let tls_start = (alloc_size - block_size) & !(align - 1);

    // Zero the entire allocation (DTV area, gap, TLS block, TCB).
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
    // Write self-pointer via kernel direct map
    let tp_kernel = block as u64 + (tls_start + total_memsz) as u64;
    unsafe { *(tp_kernel as *mut u64) = tp_user; }

    // Set up DTV at the start of the allocation.
    // DTV entries point to the start of each module's TLS data (user-visible addresses).
    let dtv_size = DTV_HEADER_SIZE + DTV_INITIAL_CAPACITY * 8;
    assert!(dtv_size < tls_start, "DTV overlaps TLS data");
    let dtv_kern = block as *mut u64;
    unsafe {
        // generation = 1 (initial)
        *dtv_kern = 1;
        // len = DTV_INITIAL_CAPACITY
        *dtv_kern.add(1) = DTV_INITIAL_CAPACITY as u64;
        // Initialize all entries as unallocated
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

// ---------------------------------------------------------------------------
// Kernel stack allocation
// ---------------------------------------------------------------------------

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

/// Release the CPU queue lock held across context_switch.
/// Called by process_start/thread_start before entering userspace.
fn scheduler_unlock() {
    unsafe { scheduler::force_unlock_current_cpu(); }
    scheduler::handle_outgoing_public();
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
        unlock = sym scheduler_unlock,
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
        unlock = sym scheduler_unlock,
    );
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_name(path: &str) -> [u8; 28] {
    let filename = path.rsplit('/').next().unwrap_or(path);
    let mut name = [0u8; 28];
    let len = filename.len().min(27);
    name[..len].copy_from_slice(&filename.as_bytes()[..len]);
    name
}

/// Build a child's FdTable from (child_fd, parent_fd) pairs.
/// Duplicates each referenced parent descriptor into the child table.
pub fn build_child_fds(pairs: &[[u32; 2]]) -> FdTable {
    let data_arc = fd_owner_data();
    let data = data_arc.lock();
    let mut fds = FdTable::new();
    for &[child_fd, parent_fd] in pairs {
        if let Some(desc) = data.fds.get(parent_fd) {
            let cloned = desc.clone();
            fd::alloc_at(&mut fds, child_fd, cloned);
        }
    }
    fds
}

/// User virtual address space starts at 1TB — well above any direct-mapped physical RAM.
const USER_VM_BASE: u64 = 0x100_0000_0000;

/// Convert an ELF virtual address to a file offset by searching PT_LOAD segments.
/// Falls back to extrapolating from the nearest segment for vaddrs outside all segments
/// (e.g. `.rela.dyn` sections the linker places outside PT_LOAD).
fn vaddr_to_file_offset(segments: &[elf::ElfSegment], vaddr: u64) -> u64 {
    for seg in segments {
        if vaddr >= seg.vaddr && vaddr < seg.vaddr + seg.filesz {
            return seg.file_offset + (vaddr - seg.vaddr);
        }
    }
    // Extrapolate from the nearest segment below this vaddr.
    // Works for PIE binaries where file_offset == vaddr (common pattern).
    let mut best: Option<&elf::ElfSegment> = None;
    for seg in segments {
        if seg.vaddr <= vaddr {
            if best.map_or(true, |b| seg.vaddr > b.vaddr) {
                best = Some(seg);
            }
        }
    }
    match best {
        Some(seg) => seg.file_offset + (vaddr - seg.vaddr),
        None => panic!("vaddr_to_file_offset: {:#x} not in or near any PT_LOAD segment", vaddr),
    }
}

/// Read a byte range from a file using its block map via the page cache.
pub(crate) fn read_file_range(backing: &dyn crate::file_backing::FileBacking, offset: u64, len: usize) -> Vec<u8> {
    let mut result = Vec::with_capacity(len);
    let mut remaining = len;
    let mut file_off = offset;
    let mut page_buf = [0u8; 4096];

    while remaining > 0 {
        let off_in_block = (file_off % 4096) as usize;
        let chunk = (4096 - off_in_block).min(remaining);

        backing.read_page(file_off - off_in_block as u64, &mut page_buf);
        result.extend_from_slice(&page_buf[off_in_block..off_in_block + chunk]);

        file_off += chunk as u64;
        remaining -= chunk;
    }

    result
}

/// Resolve a single exe TPOFF relocation entry to a pre-computed i64 value.
/// Handles both r_sym == 0 (simple offset) and r_sym != 0 (cross-library lookup).
fn resolve_exe_tpoff(
    r_sym: u32,
    r_addend: i64,
    exe_base_offset: usize,
    total_memsz: usize,
    segments: &[elf::ElfSegment],
    symtab_vaddr: u64,
    backing: &dyn crate::file_backing::FileBacking,
    dynstr_data: &[u8],
    tls_info: &elf::TlsModuleInfo,
) -> i64 {
    if r_sym == 0 {
        return exe_base_offset as i64 + r_addend - total_memsz as i64;
    }

    let symtab_file_off = vaddr_to_file_offset(segments, symtab_vaddr);
    let sym_data = read_file_range(backing, symtab_file_off + r_sym as u64 * elf::SYM_SIZE as u64, elf::SYM_SIZE);
    if sym_data.len() < elf::SYM_SIZE {
        return exe_base_offset as i64 + r_addend - total_memsz as i64;
    }
    let sym = elf::read_sym(&sym_data, 0);

    if sym.st_shndx != 0 {
        exe_base_offset as i64 + sym.st_value as i64 + r_addend - total_memsz as i64
    } else {
        let sym_name = elf::sym_name(&sym, dynstr_data);

        // Search loaded libraries for the defining TLS symbol
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
fn insert_elf_regions(
    addr_space: &mut crate::mm::paging::AddressSpace,
    layout: &elf::ElfLayout,
    base: u64,
    backing: &Arc<dyn crate::file_backing::FileBacking>,
) {
    use crate::vma::{Region, RegionKind};

    for seg in &layout.segments {
        let seg_start = (base + seg.vaddr) & !0xFFF;
        let seg_end = (base + seg.vaddr + seg.memsz + 0xFFF) & !0xFFF;

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
        let template = exe_tls_template
            .map(|buf| unsafe { crate::mm::KernelSlice::from_raw(buf.ptr(), layout.tls_filesz) });
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
    let path = argv[0];
    let t0 = crate::clock::nanos_since_boot();

    // 1. Open file backing from VFS (follows symlinks)
    let backing: Arc<dyn crate::file_backing::FileBacking> = match vfs::lock().open_backing(path) {
        Some(b) => b,
        None => {
            log!("spawn: {}: not found", path);
            return Err(SyscallError::NotFound);
        }
    };

    // 2. Read first few blocks for ELF headers
    let header_size = 4096.min(backing.file_size() as usize);
    let header_data = read_file_range(backing.as_ref(), 0, header_size);

    // 3. Parse ELF layout from headers
    let layout = match elf::parse_layout(&header_data) {
        Ok(l) => l,
        Err(msg) => {
            log!("spawn: {}: {}", path, msg);
            return Err(SyscallError::InvalidArgument);
        }
    };

    // 3b. Parse PT_DYNAMIC from block map (not available in the header buffer)
    let dyn_info = if let Some((dyn_off, _, dyn_size)) = layout.dynamic {
        let dyn_data = read_file_range(backing.as_ref(), dyn_off, dyn_size as usize);
        elf::parse_dynamic(&dyn_data)
    } else {
        elf::DynamicInfo::empty()
    };

    let t1 = crate::clock::nanos_since_boot();

    // 4. Choose base address in user virtual space
    let base = USER_VM_BASE - layout.vaddr_min;

    // 6. Read and parse relocation tables from block map
    let rela_data = if dyn_info.rela_size > 0 {
        let rela_file_off = vaddr_to_file_offset(&layout.segments, dyn_info.rela_vaddr);
        read_file_range(backing.as_ref(), rela_file_off, dyn_info.rela_size as usize)
    } else if layout.dynamic.is_none() {
        // No PT_DYNAMIC — fall back to finding .rela.dyn from section headers
        if let Some((shoff, shnum, shentsize)) = layout.section_headers {
            let shdr_data = read_file_range(backing.as_ref(), shoff, shnum as usize * shentsize as usize);
            let bk = backing.as_ref();
            if let Some((rela_off, rela_size)) = elf::find_rela_dyn_from_sections(
                &shdr_data, shentsize, &|off, len| read_file_range(bk, off, len),
            ) {
                read_file_range(backing.as_ref(), rela_off, rela_size as usize)
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
        let jmprel_file_off = vaddr_to_file_offset(&layout.segments, dyn_info.jmprel_vaddr);
        read_file_range(backing.as_ref(), jmprel_file_off, dyn_info.jmprel_size as usize)
    } else {
        Vec::new()
    };
    let parsed_relas = elf::parse_rela_entries(&rela_data, &jmprel_data);

    // Start building the relocation index with RELATIVE entries (pre-computed: base + addend)
    let mut reloc_index = elf::RelocationIndex::new();
    for &(r_offset, r_addend) in &parsed_relas.relative {
        reloc_index.add_u64(r_offset, (base as i64 + r_addend) as u64);
    }

    let t2 = crate::clock::nanos_since_boot();

    // 7. Load shared libraries from block map (no full binary read)
    // Read DT_STRTAB from block map to get library names
    let (mut loaded_libs, lib_paths) = if !dyn_info.needed_strtab_offsets.is_empty() && dyn_info.strsz > 0 {
        let strtab_file_off = vaddr_to_file_offset(&layout.segments, dyn_info.strtab_vaddr);
        let strtab_data = read_file_range(backing.as_ref(), strtab_file_off, dyn_info.strsz as usize);

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

            // Check the shared library cache first
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
                Ok((lib, rw_vaddr, rw_end_vaddr)) => {
                    let t_load1 = crate::clock::nanos_since_boot();
                    log!("dynamic: loaded {} base={:#x} ({} syms, {}ms)",
                        lib_name, lib.phys_base, lib.sym_count,
                        (t_load1 - t_load0) / 1_000_000);
                    let lib = elf::cache_loaded_lib_pub(&lib_path, lib, rw_vaddr, rw_end_vaddr);
                    lib_paths_vec.push(lib_path);
                    libs.push(lib);
                }
                Err(e) => {
                    log!("spawn: {}: failed to load {}: {}", path, lib_name, e);
                    return Err(SyscallError::NotFound);
                }
            }
        }

        // 7b. Read exe .dynsym/.dynstr from block map for exe sym map
        if !libs.is_empty() {
            let dynstr_file_off = vaddr_to_file_offset(&layout.segments, dyn_info.strtab_vaddr);
            let dynstr_data = read_file_range(backing.as_ref(), dynstr_file_off, dyn_info.strsz as usize);

            // Determine .dynsym entry count via GNU hash table or SYMTAB/STRTAB gap
            let sym_count = if dyn_info.gnu_hash_vaddr != 0 {
                let gnu_hash_file_off = vaddr_to_file_offset(&layout.segments, dyn_info.gnu_hash_vaddr);
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
                let symtab_file_off = vaddr_to_file_offset(&layout.segments, dyn_info.symtab_vaddr);
                let dynsym_data = read_file_range(backing.as_ref(), symtab_file_off, sym_count * elf::SYM_SIZE);
                elf::build_exe_sym_map(&dynsym_data, &dynstr_data, sym_count, UserAddr::new(base))
            } else {
                hashbrown::HashMap::new()
            };

            // If .dynsym has no defined symbols, fall back to .symtab from section headers.
            // This handles PIE executables that don't export symbols via --export-dynamic.
            if exe_sym_map.is_empty() {
                if let Some((shoff, shnum, shentsize)) = layout.section_headers {
                    let shdr_data = read_file_range(backing.as_ref(), shoff, shnum as usize * shentsize as usize);
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
    insert_elf_regions(&mut child_pt.lock(), &layout, base, &backing);

    // 8b. Map shared libraries and assign virtual addresses.
    // This MUST happen BEFORE relocation processing so that user_base is correct
    // when GOT entries are written (RELATIVE: user_base + addend, GLOB_DAT: user_base + sym.st_value).
    for lib in &mut loaded_libs {
        match &lib.memory {
            elf::LibMemory::Owned(alloc) => {
                let phys = DirectMap::phys_of(alloc.ptr());
                let (vaddr, _) = vma_map(&child_pt, phys, alloc.size() as u64)
                    .expect("spawn: out of virtual address space for lib");
                let delta = vaddr.raw() as i64 - lib.user_base.raw() as i64;
                lib.user_base = vaddr;
                lib.user_end = (lib.user_end as i64 + delta) as u64;
            }
            elf::LibMemory::Shared { rw_alloc, cached_image, rw_offset, .. } => {
                let cached_phys = cached_image.phys();
                let (lib_vaddr, _) = vma_map(&child_pt, cached_phys, cached_image.size() as u64)
                    .expect("spawn: out of virtual address space for lib");
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
        // Rebuild exe_sym_map (we need it for bind relocs and exe GLOB_DAT)
        let dynstr_file_off = vaddr_to_file_offset(&layout.segments, dyn_info.strtab_vaddr);
        let dynstr_data = if dyn_info.strsz > 0 {
            read_file_range(backing.as_ref(), dynstr_file_off, dyn_info.strsz as usize)
        } else {
            Vec::new()
        };

        let sym_count = if dyn_info.gnu_hash_vaddr != 0 {
            let gnu_hash_file_off = vaddr_to_file_offset(&layout.segments, dyn_info.gnu_hash_vaddr);
            let gnu_hash_data = read_file_range(backing.as_ref(), gnu_hash_file_off, 64 * 1024);
            elf::gnu_hash_sym_count_from_data(&gnu_hash_data)
        } else if dyn_info.symtab_vaddr != 0 && dyn_info.strtab_vaddr > dyn_info.symtab_vaddr {
            ((dyn_info.strtab_vaddr - dyn_info.symtab_vaddr) / 24) as usize
        } else {
            0
        };

        let exe_sym_map = if sym_count > 0 {
            let symtab_file_off = vaddr_to_file_offset(&layout.segments, dyn_info.symtab_vaddr);
            let dynsym_data = read_file_range(backing.as_ref(), symtab_file_off, sym_count * elf::SYM_SIZE);
            elf::build_exe_sym_map(&dynsym_data, &dynstr_data, sym_count, UserAddr::new(base))
        } else {
            hashbrown::HashMap::new()
        };

        // Resolve lib bind relocs against exe symbols (NOW user_base is correct)
        for lib in &loaded_libs {
            elf::resolve_lib_bind_relocs_pub(lib, &exe_sym_map, &loaded_libs);
        }

        // Resolve exe GLOB_DAT entries against loaded libs
        let symtab_file_off = vaddr_to_file_offset(&layout.segments, dyn_info.symtab_vaddr);
        for &(r_offset, r_sym, _r_addend) in &parsed_relas.glob_dat {
            if r_sym == 0 { continue; }
            let sym_data = read_file_range(backing.as_ref(), symtab_file_off + r_sym as u64 * elf::SYM_SIZE as u64, elf::SYM_SIZE);
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
        pt.map_range(stack_vaddr, stack_pages.phys(), USER_STACK_SIZE as u64, true);
        pt.insert_region(stack_vaddr, crate::vma::Region {
            size: USER_STACK_SIZE as u64,
            writable: true,
            kind: crate::vma::RegionKind::Anonymous,
        });
    }

    // 10. TLS setup — read exe TLS template from page cache, build multi-module layout
    let exe_tls_template = if layout.tls_memsz > 0 {
        let tls_file_off = vaddr_to_file_offset(&layout.segments, layout.tls_vaddr);
        let tls_data = read_file_range(backing.as_ref(), tls_file_off, layout.tls_filesz);
        let tls_buf = OwnedAlloc::new(layout.tls_memsz, 16).expect("TLS template alloc");
        unsafe {
            core::ptr::copy_nonoverlapping(tls_data.as_ptr(), tls_buf.ptr(), layout.tls_filesz);
            if layout.tls_memsz > layout.tls_filesz {
                core::ptr::write_bytes(tls_buf.ptr().add(layout.tls_filesz), 0, layout.tls_memsz - layout.tls_filesz);
            }
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

        // Read exe .dynsym/.dynstr for resolving named TPOFF symbols
        let dynstr_data = if dyn_info.strsz > 0 {
            let dynstr_file_off = vaddr_to_file_offset(&layout.segments, dyn_info.strtab_vaddr);
            read_file_range(backing.as_ref(), dynstr_file_off, dyn_info.strsz as usize)
        } else {
            Vec::new()
        };

        for &(r_offset, r_sym, r_addend) in &parsed_relas.tpoff64 {
            let tpoff = resolve_exe_tpoff(
                r_sym, r_addend, exe_base_offset, tls_total_memsz,
                &layout.segments, dyn_info.symtab_vaddr, backing.as_ref(), &dynstr_data, &tls_info,
            );
            reloc_index.add_u64(r_offset, tpoff as u64);
        }
        for &(r_offset, r_sym, r_addend) in &parsed_relas.tpoff32 {
            let tpoff = resolve_exe_tpoff(
                r_sym, r_addend, exe_base_offset, tls_total_memsz,
                &layout.segments, dyn_info.symtab_vaddr, backing.as_ref(), &dynstr_data, &tls_info,
            );
            reloc_index.add_i32(r_offset, tpoff as i32);
        }
    }

    // Finalize reloc index (sort all entries)
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
    let (tls_vaddr, _) = vma_map(&child_pt, tls_phys, tls_alloc.size() as u64)
        .expect("spawn: out of virtual address space for TLS");
    let tls_rebase = tls_vaddr.raw() as i64 - tls_phys as i64;
    let fs_base = (fs_base as i64 + tls_rebase) as u64;
    // Fix self-pointer (TCB[0]) and DTV pointer (TCB[8]) in the TLS block
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
                Arc::clone(table.get_process(ppid).unwrap().process_data())
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
        tls_pages: Some(tls_alloc),
        stack_pages: Some(stack_pages),
        user_stack_base: user_stack.base(),
        user_stack_size: user_stack.size(),
        syscall_counts: [0; 64],
        syscall_total: 0,
        syscall_total_ns: 0,
    }));

    let mut guard = PROCESS_TABLE.lock();
    let table = guard.as_mut().unwrap();
    let tid = table.alloc_tid();
    let mut threads = hashbrown::HashMap::new();
    threads.insert(tid, ThreadEntry::new(thread_data));
    let pid = table.insert_process(|pid| ProcessEntry::new(
        pid,
        parent,
        make_name(path),
        proc_data,
        Arc::new(Lock::new(syms)),
        tid,
        threads,
    ));
    drop(guard);

    let ctx = scheduler::ThreadCtx {
        tid,
        process: pid,
        kernel_stack: ks_alloc,
        kernel_rsp: ks_rsp,
        address_space: Some(child_pt.clone()),
        fs_base,
        cpu_ns: 0,
        scheduled_at: 0,
        blocked_on: None,
        deadline: 0,
        blocked_since: 0,
        enqueued_at: 0,
        accounting: scheduler::ThreadAccounting::default(),
    };
    scheduler::enqueue_new(ctx);

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
    fds.insert_at(0, Descriptor::SerialConsole);
    fds.insert_at(1, Descriptor::SerialConsole);
    fds.insert_at(2, Descriptor::SerialConsole);
    spawn(argv, fds, None, Vec::new()).expect("spawn_kernel: failed to spawn")
}
