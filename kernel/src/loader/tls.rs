//! Building a thread's TLS block.
//!
//! The layout arithmetic is `toyos_elf::tls`, which is pure and host-tested
//! over the whole space a file can name; what is here is the allocation, the
//! template copies and the DTV the kernel writes in front of the data.

use crate::elf::TlsModule;
use crate::mm::KernelSlice;
use crate::process::{OwnedAlloc, PageAlloc};
use crate::DirectMap;
use toyos_elf::Layout;

const TCB_SIZE: usize = 64;
/// Module entries a thread's DTV can hold. `SYS_TLS_ALLOC_BLOCK` refuses a
/// module id above it, because there is nowhere to record the answer.
pub const DTV_INITIAL_CAPACITY: usize = 64;
/// Generation (8) + length (8).
const DTV_HEADER_SIZE: usize = 16;
const DTV_BYTES: usize = DTV_HEADER_SIZE + DTV_INITIAL_CAPACITY * 8;
/// A DTV slot for a module whose block has not been allocated yet.
const DTV_UNALLOCATED: u64 = !0u64;

/// One module's TLS area, for a thread that has only the executable's.
pub fn setup_tls(
    tls_template: Option<KernelSlice>,
    tls_memsz: usize,
    tls_align: usize,
) -> Option<(PageAlloc, u64)> {
    setup_combined_tls(
        &[TlsModule {
            template: tls_template,
            memsz: tls_memsz,
            base_offset: 0,
            module_id: 1,
            is_static: true,
        }],
        tls_memsz,
        tls_align,
    )
}

/// Allocate and populate one thread's TLS block for every static module.
///
/// x86-64 variant II, with the DTV in front:
///
/// ```text
/// [DTV] [padding] [TLS data (.tdata + .tbss)] [TCB]
///                                              ^ TP (FS base)
/// ```
///
/// TCB: `TP+0x00` is the self-pointer the ABI requires, `TP+0x08` the DTV
/// pointer, both user-visible physical addresses — `spawn` rebases them once
/// the block has a virtual address. DTV: generation, length, then one entry per
/// module id.
///
/// `None` for a layout no allocation can hold.
pub fn setup_combined_tls(
    modules: &[TlsModule],
    total_memsz: usize,
    tls_align: usize,
) -> Option<(PageAlloc, u64)> {
    let plan = toyos_elf::tls::plan(
        total_memsz,
        tls_align,
        TCB_SIZE,
        DTV_BYTES,
        crate::mm::PAGE_2M as usize,
    )?;
    let page_alloc = PageAlloc::new(plan.alloc_size, crate::mm::pmm::Category::InitTls)?;
    let block = page_alloc.ptr();

    // SAFETY: `block` is `page_alloc.ptr()`, a fresh `PageAlloc::new
    // (plan.alloc_size, ...)` immediately above — valid and exclusively
    // owned for exactly `plan.alloc_size` bytes, and not yet published
    // anywhere else.
    unsafe {
        core::ptr::write_bytes(block, 0, plan.alloc_size);
    }

    for module in modules.iter().filter(|m| m.is_static) {
        if let Some(template) = &module.template {
            // SAFETY: `template.base()` is valid for `template.size()` bytes
            // (a `KernelSlice`, bounds-checked when it was built — see
            // `KernelSlice::as_slice`'s `# Safety` for the same class of
            // guarantee). The destination stays inside `block`'s
            // `plan.alloc_size`-byte allocation by `toyos_elf::tls`'s own
            // contract, chained: `template.size() <= module.memsz` (ELF's
            // own `filesz <= memsz`), `module.base_offset + module.memsz <=
            // total_memsz` (`place_module`'s contract, see
            // `build_tls_layout`'s doc), and `plan.tls_start + total_memsz
            // <= plan.alloc_size` (`plan`'s own formula: `tls_start =
            // (alloc_size - block_size) & !(align - 1) <= alloc_size -
            // block_size`, and `block_size >= total_memsz`). `block` is
            // freshly zeroed above and not yet published.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    template.base(),
                    block.add(plan.tls_start + module.base_offset),
                    template.size(),
                );
            }
        }
    }

    let block_phys = DirectMap::from_ptr(block).phys();
    let tp_user = block_phys + plan.tp_offset as u64;
    // SAFETY: `plan.tp_offset = tls_start + total_memsz` (the crate's own
    // field doc: "Where the thread pointer goes") is where the TCB starts,
    // and `plan()`'s `alloc_size` formula reserves `TCB_SIZE` bytes after it
    // — so `block.add(plan.tp_offset)` plus the 16 bytes the block below
    // writes through it stays inside `block`'s allocation.
    let tp_kernel = unsafe { block.add(plan.tp_offset) } as *mut u64;
    // SAFETY: `tp_kernel`'s bound was just established above; two `u64`
    // writes (16 of the `TCB_SIZE` = 64 bytes reserved there). Same
    // exclusivity as the zero/copy above — `block` is not yet published.
    unsafe {
        *tp_kernel = tp_user;
        *tp_kernel.add(1) = block_phys;
    }

    let dtv = block as *mut u64;
    // SAFETY: `dtv = block`, and every write below lands in `[0,
    // DTV_BYTES)` (`DTV_HEADER_SIZE` plus `DTV_INITIAL_CAPACITY` eight-byte
    // slots — exactly the `dtv_bytes` `plan()` was called with above).
    // `plan()`'s own formula (`alloc_size = align_up(block_size + dtv_bytes
    // + align, granule)`) guarantees at least that much room at the front of
    // `block`. The per-module loop checks `idx <= DTV_INITIAL_CAPACITY`
    // before every write. Same exclusivity as the rest of this function.
    unsafe {
        *dtv = 1;
        *dtv.add(1) = DTV_INITIAL_CAPACITY as u64;
        for i in 0..DTV_INITIAL_CAPACITY {
            *dtv.add(2 + i) = DTV_UNALLOCATED;
        }
        // Static modules only. A `dlopen`ed module's slot stays unallocated
        // until `__tls_get_addr` asks for it.
        for module in modules.iter().filter(|m| m.is_static) {
            let idx = module.module_id as usize;
            if idx > 0 && idx <= DTV_INITIAL_CAPACITY {
                *dtv.add(2 + idx - 1) = block_phys + (plan.tls_start + module.base_offset) as u64;
            }
        }
    }

    Some((page_alloc, tp_user))
}

/// Build one thread's TLS block and map it into the child address space.
///
/// The block is filled with *physical* addresses because it has no virtual
/// address until it is mapped, so the thread pointer, the DTV pointer and every
/// filled DTV entry are rebased by one delta afterwards. `None` when the block
/// cannot be allocated or the address space has no room for it.
pub fn map_block(
    child_pt: &crate::process::PageTables,
    modules: &[TlsModule],
    total_memsz: usize,
    max_align: usize,
) -> Option<(crate::process::MappedPages, u64)> {
    let (alloc, fs_base) = if total_memsz > 0 {
        setup_combined_tls(modules, total_memsz, max_align)?
    } else {
        setup_tls(None, 0, 1)?
    };

    let phys = alloc.phys();
    let (vaddr, _) = crate::process::vma_map(child_pt, phys, alloc.size() as u64,
        crate::mm::paging::Prot::ReadWrite)?;
    let rebase = vaddr.raw() as i64 - phys as i64;
    let fs_base = (fs_base as i64 + rebase) as u64;
    // SAFETY: `phys`/`alloc` are the block `setup_combined_tls`/`setup_tls`
    // just built and zeroed — `(fs_base - vaddr.raw())` reduces to the same
    // `plan.tp_offset` that function already bounded against `alloc_size`
    // (algebraically: `fs_base = block_phys + plan.tp_offset + rebase`,
    // `rebase = vaddr - phys`, and `block_phys == phys`, so `fs_base -
    // vaddr.raw() == plan.tp_offset`), and `dtv`/its length-prefixed entries
    // are the same `[0, DTV_BYTES)` region that function wrote. The direct
    // map (`DirectMap::from_phys`) is the kernel's own view of these
    // physical pages; `vma_map` just mapped the same pages into the child's
    // address space, but the child has not been scheduled yet (this runs on
    // the spawning thread, before `SYS_SPAWN` hands the new process to the
    // scheduler), so nothing else can be reading or writing through either
    // view concurrently.
    unsafe {
        let block = DirectMap::from_phys(phys).as_mut_ptr::<u8>();
        let tp = block.add((fs_base - vaddr.raw()) as usize) as *mut u64;
        *tp = fs_base;
        *tp.add(1) = (*tp.add(1) as i64 + rebase) as u64;
        let dtv = block as *mut u64;
        let dtv_len = *dtv.add(1) as usize;
        for i in 0..dtv_len {
            let entry = *dtv.add(2 + i);
            if entry != DTV_UNALLOCATED && entry != 0 {
                *dtv.add(2 + i) = (entry as i64 + rebase) as u64;
            }
        }
    }
    Some((crate::process::MappedPages::new(vaddr, alloc), fs_base))
}

/// Place every startup module in one combined block: libraries first, then the
/// executable.
///
/// Module id 1 is the executable's by convention — `__tls_get_addr` and the
/// DTV both depend on it — so the libraries take 2 upwards even though they
/// are laid out first.
///
/// Returns the modules, the block's total size, the strictest alignment any of
/// them asked for, and the next free module id. `None` when the modules do not
/// fit one block, which is a refusal rather than a module quietly left out: a
/// missing module is relocations resolving against a block that is not there.
pub fn build_tls_layout(
    loaded_libs: &[crate::elf::LoadedLib],
    layout: &Layout,
    exe_tls_template: Option<&OwnedAlloc>,
) -> Option<(alloc::vec::Vec<TlsModule>, usize, usize, u64)> {
    let exe = layout.tls.filter(|t| t.memsz > 0);
    let libs = loaded_libs.iter().filter(|lib| lib.tls_memsz > 0);

    let mut modules = alloc::vec::Vec::with_capacity(loaded_libs.len() + 1);
    let mut cursor = 0usize;
    let mut max_align = 1usize;
    let mut next_module_id = 2u64;

    for lib in libs {
        let (base_offset, next) = toyos_elf::tls::place_module(cursor, lib.tls_memsz)?;
        cursor = next;
        max_align = max_align.max(lib.tls_align);
        modules.push(TlsModule {
            template: lib.tls_template,
            memsz: lib.tls_memsz,
            base_offset,
            module_id: next_module_id,
            is_static: true,
        });
        next_module_id += 1;
    }

    if let Some(tls) = exe {
        let (base_offset, next) = toyos_elf::tls::place_module(cursor, tls.memsz as usize)?;
        cursor = next;
        max_align = max_align.max(tls.align as usize);
        modules.push(TlsModule {
            template: exe_tls_template.map(|buf| buf.slice(tls.filesz as usize)),
            memsz: tls.memsz as usize,
            base_offset,
            module_id: 1,
            is_static: true,
        });
    }

    Some((modules, cursor, max_align, next_module_id))
}
