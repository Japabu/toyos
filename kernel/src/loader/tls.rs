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

    unsafe {
        core::ptr::write_bytes(block, 0, plan.alloc_size);
    }

    for module in modules.iter().filter(|m| m.is_static) {
        if let Some(template) = &module.template {
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
    let tp_kernel = unsafe { block.add(plan.tp_offset) } as *mut u64;
    unsafe {
        *tp_kernel = tp_user;
        *tp_kernel.add(1) = block_phys;
    }

    let dtv = block as *mut u64;
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
