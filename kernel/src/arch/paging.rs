use core::alloc::Layout;
use core::ptr::null_mut;

use alloc::alloc::{alloc_zeroed, dealloc};

use super::{apic, cpu};
use crate::MemoryMapEntry;

const PAGE_PRESENT: u64 = 1 << 0;
const PAGE_WRITE: u64 = 1 << 1;
const PAGE_USER: u64 = 1 << 2;
const PAGE_SIZE_BIT: u64 = 1 << 7; // 2MB/1GB large page
const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

const PAGE_4K: u64 = 4096;
pub const PAGE_2M: u64 = 2 * 1024 * 1024;

/// Round `size` up to the next 2MB boundary.
pub const fn align_2m(size: usize) -> usize {
    (size + PAGE_2M as usize - 1) & !(PAGE_2M as usize - 1)
}

/// Kernel PML4 template. All per-process PML4s are deep-cloned from this.
/// Written once during init(), read-only afterwards (except map_kernel at boot).
static mut KERNEL_PML4: *mut u64 = null_mut();

/// How many PML4 entries the kernel identity map uses (set during init).
static mut KERNEL_PML4_ENTRIES: usize = 0;

/// Allocate a zeroed, page-aligned 4KB page for page table structures.
fn alloc_page() -> *mut u64 {
    let layout = Layout::from_size_align(PAGE_4K as usize, PAGE_4K as usize).unwrap();
    let ptr = unsafe { alloc_zeroed(layout) };
    assert!(!ptr.is_null(), "paging: out of memory for page table");
    ptr as *mut u64
}

fn free_page(ptr: *mut u64) {
    let layout = Layout::from_size_align(PAGE_4K as usize, PAGE_4K as usize).unwrap();
    unsafe { dealloc(ptr as *mut u8, layout) };
}

/// Build kernel page tables: identity-map all physical memory as kernel-only
/// using 2MB large pages, then load CR3.
pub fn init(memory_map: &[MemoryMapEntry]) {
    let mut max_addr: u64 = 4 * 1024 * 1024 * 1024; // 4GB minimum for MMIO
    for entry in memory_map {
        if entry.end > max_addr {
            max_addr = entry.end;
        }
    }
    max_addr = (max_addr + PAGE_2M - 1) & !(PAGE_2M - 1);

    let pml4 = alloc_page();

    let mut addr: u64 = 0;
    while addr < max_addr {
        let pml4_idx = ((addr >> 39) & 0x1FF) as usize;
        let pdpt_idx = ((addr >> 30) & 0x1FF) as usize;
        let pd_idx = ((addr >> 21) & 0x1FF) as usize;

        let pdpt = unsafe { get_or_create(pml4, pml4_idx, PAGE_PRESENT | PAGE_WRITE) };
        let pd = unsafe { get_or_create(pdpt, pdpt_idx, PAGE_PRESENT | PAGE_WRITE) };
        unsafe {
            pd.add(pd_idx).write(addr | PAGE_PRESENT | PAGE_WRITE | PAGE_SIZE_BIT);
        }

        addr += PAGE_2M;
    }

    let entries = ((max_addr + (1u64 << 39) - 1) >> 39) as usize;
    unsafe {
        KERNEL_PML4 = pml4;
        KERNEL_PML4_ENTRIES = entries;
        cpu::write_cr3(pml4 as u64);
    }
}

/// Get or create a next-level page table at the given index.
unsafe fn get_or_create(table: *mut u64, index: usize, flags: u64) -> *mut u64 {
    let entry = table.add(index).read();
    if entry & PAGE_PRESENT != 0 {
        let updated = entry | (flags & (PAGE_PRESENT | PAGE_WRITE | PAGE_USER));
        if updated != entry {
            table.add(index).write(updated);
        }
        (entry & ADDR_MASK) as *mut u64
    } else {
        let new_table = alloc_page();
        table.add(index).write(new_table as u64 | flags);
        new_table
    }
}

/// Physical address of the kernel PML4 template. Used for idle/exit CR3.
pub fn kernel_cr3() -> u64 {
    unsafe { KERNEL_PML4 as u64 }
}

/// Create a per-process PML4 by deep-cloning the kernel page table hierarchy.
/// Each process gets its own PML4 -> PDPT -> PD chain so USER bits are independent.
pub fn create_user_pml4() -> *mut u64 {
    let kernel_pml4 = unsafe { KERNEL_PML4 };
    let entries = unsafe { KERNEL_PML4_ENTRIES };
    assert!(!kernel_pml4.is_null(), "paging: not initialized");

    let pml4 = alloc_page();

    for pml4_idx in 0..entries {
        let pml4e = unsafe { kernel_pml4.add(pml4_idx).read() };
        if pml4e & PAGE_PRESENT == 0 {
            continue;
        }
        let kernel_pdpt = (pml4e & ADDR_MASK) as *mut u64;
        let pdpt = alloc_page();

        for pdpt_idx in 0..512 {
            let pdpte = unsafe { kernel_pdpt.add(pdpt_idx).read() };
            if pdpte & PAGE_PRESENT == 0 {
                continue;
            }
            let kernel_pd = (pdpte & ADDR_MASK) as *mut u64;
            let pd = alloc_page();

            // Copy all 512 PD entries (2MB large pages)
            unsafe { core::ptr::copy_nonoverlapping(kernel_pd, pd, 512); }

            unsafe {
                pdpt.add(pdpt_idx).write(pd as u64 | (pdpte & !ADDR_MASK));
            }
        }

        unsafe {
            pml4.add(pml4_idx).write(pdpt as u64 | (pml4e & !ADDR_MASK));
        }
    }

    pml4
}

/// Free a per-process PML4 and all its cloned PDPT/PD tables.
/// Does NOT free the underlying 2MB physical pages.
pub fn free_user_page_tables(pml4: *mut u64) {
    let entries = unsafe { KERNEL_PML4_ENTRIES };

    for pml4_idx in 0..entries {
        let pml4e = unsafe { pml4.add(pml4_idx).read() };
        if pml4e & PAGE_PRESENT == 0 {
            continue;
        }
        let pdpt = (pml4e & ADDR_MASK) as *mut u64;

        for pdpt_idx in 0..512 {
            let pdpte = unsafe { pdpt.add(pdpt_idx).read() };
            if pdpte & PAGE_PRESENT == 0 {
                continue;
            }
            let pd = (pdpte & ADDR_MASK) as *mut u64;
            free_page(pd);
        }

        free_page(pdpt);
    }

    free_page(pml4);
}

/// Set USER bit on 2MB pages in a specific PML4. Also sets USER on parent entries.
pub fn map_user_in(pml4: *mut u64, addr: u64, size: u64) {
    let start = addr & !(PAGE_2M - 1);
    let end = (addr + size + PAGE_2M - 1) & !(PAGE_2M - 1);
    let mut cur = start;

    while cur < end {
        let pml4_idx = ((cur >> 39) & 0x1FF) as usize;
        let pdpt_idx = ((cur >> 30) & 0x1FF) as usize;
        let pd_idx = ((cur >> 21) & 0x1FF) as usize;

        unsafe {
            let pml4e = pml4.add(pml4_idx).read();
            assert!(pml4e & PAGE_PRESENT != 0, "map_user: PML4 not present for {:#x}", cur);
            pml4.add(pml4_idx).write(pml4e | PAGE_USER);

            let pdpt = (pml4e & ADDR_MASK) as *mut u64;
            let pdpte = pdpt.add(pdpt_idx).read();
            assert!(pdpte & PAGE_PRESENT != 0, "map_user: PDPT not present for {:#x}", cur);
            pdpt.add(pdpt_idx).write(pdpte | PAGE_USER);

            let pd = (pdpte & ADDR_MASK) as *mut u64;
            let pde = pd.add(pd_idx).read();
            assert!(pde & PAGE_PRESENT != 0, "map_user: PD not present for {:#x}", cur);
            assert!(pde & PAGE_SIZE_BIT != 0, "map_user: expected 2MB page at {:#x}", cur);
            pd.add(pd_idx).write(pde | PAGE_USER);
        }

        cur += PAGE_2M;
    }
}

/// Set USER bit on 2MB pages in the current process's page tables (via CR3).
pub fn map_user(addr: u64, size: u64) {
    let pml4 = cpu::read_cr3() as *mut u64;
    map_user_in(pml4, addr, size);
    cpu::flush_tlb();
    apic::tlb_shootdown();
}

/// Clear USER bit on 2MB pages in a specific PML4.
pub fn unmap_user(pml4: *mut u64, addr: u64, size: u64) {
    let start = addr & !(PAGE_2M - 1);
    let end = (addr + size + PAGE_2M - 1) & !(PAGE_2M - 1);
    let mut cur = start;

    while cur < end {
        let pml4_idx = ((cur >> 39) & 0x1FF) as usize;
        let pdpt_idx = ((cur >> 30) & 0x1FF) as usize;
        let pd_idx = ((cur >> 21) & 0x1FF) as usize;

        unsafe {
            let pml4e = pml4.add(pml4_idx).read();
            if pml4e & PAGE_PRESENT == 0 { cur += PAGE_2M; continue; }
            let pdpt = (pml4e & ADDR_MASK) as *mut u64;

            let pdpte = pdpt.add(pdpt_idx).read();
            if pdpte & PAGE_PRESENT == 0 { cur += PAGE_2M; continue; }
            let pd = (pdpte & ADDR_MASK) as *mut u64;

            let pde = pd.add(pd_idx).read();
            if pde & PAGE_PRESENT != 0 && pde & PAGE_SIZE_BIT != 0 {
                pd.add(pd_idx).write(pde & !PAGE_USER);
            }
        }

        cur += PAGE_2M;
    }
}

/// Check if every 2MB page in a range has PAGE_USER set in the current CR3.
pub fn is_user_mapped(addr: u64, size: u64) -> bool {
    if size == 0 {
        return true;
    }
    let Some(end_addr) = addr.checked_add(size) else { return false };

    let pml4 = cpu::read_cr3() as *const u64;
    let start = addr & !(PAGE_2M - 1);
    let end = (end_addr + PAGE_2M - 1) & !(PAGE_2M - 1);
    let mut cur = start;

    while cur < end {
        let pml4_idx = ((cur >> 39) & 0x1FF) as usize;
        let pdpt_idx = ((cur >> 30) & 0x1FF) as usize;
        let pd_idx = ((cur >> 21) & 0x1FF) as usize;

        unsafe {
            let pml4e = pml4.add(pml4_idx).read();
            if pml4e & PAGE_PRESENT == 0 || pml4e & PAGE_USER == 0 {
                return false;
            }
            let pdpt = (pml4e & ADDR_MASK) as *const u64;

            let pdpte = pdpt.add(pdpt_idx).read();
            if pdpte & PAGE_PRESENT == 0 || pdpte & PAGE_USER == 0 {
                return false;
            }
            let pd = (pdpte & ADDR_MASK) as *const u64;

            let pde = pd.add(pd_idx).read();
            if pde & PAGE_PRESENT == 0 || pde & PAGE_USER == 0 {
                return false;
            }
        }

        cur += PAGE_2M;
    }

    true
}

/// Identity-map an MMIO region as kernel-only using 2MB large pages.
/// Only call during boot before any processes exist.
pub fn map_kernel(addr: u64, size: u64) {
    let pml4 = unsafe { KERNEL_PML4 };
    assert!(!pml4.is_null(), "paging: not initialized");

    let start = addr & !(PAGE_2M - 1);
    let end = (addr + size + PAGE_2M - 1) & !(PAGE_2M - 1);
    let mut cur = start;

    while cur < end {
        let pml4_idx = ((cur >> 39) & 0x1FF) as usize;
        let pdpt_idx = ((cur >> 30) & 0x1FF) as usize;
        let pd_idx = ((cur >> 21) & 0x1FF) as usize;

        unsafe {
            let pdpt = get_or_create(pml4, pml4_idx, PAGE_PRESENT | PAGE_WRITE);
            let pd = get_or_create(pdpt, pdpt_idx, PAGE_PRESENT | PAGE_WRITE);
            let pde = pd.add(pd_idx).read();
            if pde & PAGE_PRESENT == 0 {
                pd.add(pd_idx).write(cur | PAGE_PRESENT | PAGE_WRITE | PAGE_SIZE_BIT);
            }
        }

        cur += PAGE_2M;
    }

    cpu::flush_tlb();
    apic::tlb_shootdown();
}

fn has(entry: u64, flag: u64) -> u8 {
    if entry & flag != 0 { 1 } else { 0 }
}

/// Dump page table entries for an address. Lock-free for crash safety.
pub fn debug_page_walk(addr: u64) {
    use crate::log;

    let pml4 = cpu::read_cr3() as *const u64;
    let pml4_idx = ((addr >> 39) & 0x1FF) as usize;
    let pdpt_idx = ((addr >> 30) & 0x1FF) as usize;
    let pd_idx = ((addr >> 21) & 0x1FF) as usize;

    log!("  Page walk for {:#x} [PML4={:#x} PML4[{}] PDPT[{}] PD[{}]]:",
        addr, pml4 as u64, pml4_idx, pdpt_idx, pd_idx);

    unsafe {
        let pml4e = pml4.add(pml4_idx).read_volatile();
        log!("    PML4E: {:#018x} P={} W={} U={}", pml4e,
            has(pml4e, PAGE_PRESENT), has(pml4e, PAGE_WRITE), has(pml4e, PAGE_USER));
        if pml4e & PAGE_PRESENT == 0 { return; }

        let pdpt = (pml4e & ADDR_MASK) as *const u64;
        let pdpte = pdpt.add(pdpt_idx).read_volatile();
        log!("    PDPTE: {:#018x} P={} W={} U={}", pdpte,
            has(pdpte, PAGE_PRESENT), has(pdpte, PAGE_WRITE), has(pdpte, PAGE_USER));
        if pdpte & PAGE_PRESENT == 0 { return; }

        let pd = (pdpte & ADDR_MASK) as *const u64;
        let pde = pd.add(pd_idx).read_volatile();
        log!("    PDE:   {:#018x} P={} W={} U={} PS={}", pde,
            has(pde, PAGE_PRESENT), has(pde, PAGE_WRITE), has(pde, PAGE_USER), has(pde, PAGE_SIZE_BIT));
        if pde & PAGE_PRESENT == 0 { return; }
        if pde & PAGE_SIZE_BIT != 0 {
            log!("    -> 2MB large page at {:#x}", pde & ADDR_MASK);
        }
    }
}
