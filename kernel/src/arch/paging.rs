use core::alloc::Layout;
use core::ptr::null_mut;

use alloc::alloc::alloc_zeroed;

use super::{apic, cpu};
use crate::MemoryMapEntry;
use crate::sync::Lock;

const PAGE_PRESENT: u64 = 1 << 0;
const PAGE_WRITE: u64 = 1 << 1;
const PAGE_USER: u64 = 1 << 2;
const PAGE_SIZE_BIT: u64 = 1 << 7; // 2MB/1GB large page
const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

const PAGE_4K: u64 = 4096;
const PAGE_2M: u64 = 2 * 1024 * 1024;

static PML4: Lock<*mut u64> = Lock::new(null_mut());

/// Allocate a zeroed, page-aligned 4KB page for page table structures.
fn alloc_page() -> *mut u64 {
    let layout = Layout::from_size_align(PAGE_4K as usize, PAGE_4K as usize).unwrap();
    let ptr = unsafe { alloc_zeroed(layout) };
    assert!(!ptr.is_null(), "paging: out of memory for page table");
    ptr as *mut u64
}

/// Build our own page tables: identity-map all physical memory as kernel-only
/// using 2MB large pages, then load CR3.
pub fn init(memory_map: &[MemoryMapEntry]) {
    // Find the highest physical address; extend to at least 4GB for MMIO
    let mut max_addr: u64 = 4 * 1024 * 1024 * 1024; // 4GB minimum
    for entry in memory_map {
        if entry.end > max_addr {
            max_addr = entry.end;
        }
    }
    // Round up to 2MB boundary
    max_addr = (max_addr + PAGE_2M - 1) & !(PAGE_2M - 1);

    let pml4 = alloc_page();

    // Fill page tables with 2MB identity-mapped large pages (kernel-only)
    let mut addr: u64 = 0;
    while addr < max_addr {
        let pml4_idx = ((addr >> 39) & 0x1FF) as usize;
        let pdpt_idx = ((addr >> 30) & 0x1FF) as usize;
        let pd_idx = ((addr >> 21) & 0x1FF) as usize;

        // Get or create PDPT
        let pdpt = unsafe { get_or_create(pml4, pml4_idx, PAGE_PRESENT | PAGE_WRITE) };
        // Get or create PD
        let pd = unsafe { get_or_create(pdpt, pdpt_idx, PAGE_PRESENT | PAGE_WRITE) };
        // Write 2MB large page entry
        unsafe {
            pd.add(pd_idx).write(addr | PAGE_PRESENT | PAGE_WRITE | PAGE_SIZE_BIT);
        }

        addr += PAGE_2M;
    }

    *PML4.lock() = pml4;
    unsafe { cpu::write_cr3(pml4 as u64) };
}

/// Get or create a next-level page table at the given index.
/// Adds `extra_flags` to existing entries (used to propagate USER upward).
unsafe fn get_or_create(table: *mut u64, index: usize, flags: u64) -> *mut u64 {
    let entry = table.add(index).read();
    if entry & PAGE_PRESENT != 0 {
        // Add any new flags (e.g. USER) to existing intermediate entry
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

/// Split a 2MB large page into 512 identity-mapped 4KB pages.
/// Returns the new PT. The PDE is updated to point to it.
unsafe fn split_2m(pd: *mut u64, pd_idx: usize) -> *mut u64 {
    let pde = pd.add(pd_idx).read();
    let base = pde & ADDR_MASK; // 2MB-aligned physical address
    let pt = alloc_page();

    // Fill 512 entries: identity-map each 4KB page, preserving PRESENT|WRITE from the large page
    for i in 0..512u64 {
        pt.add(i as usize).write(base + i * PAGE_4K | PAGE_PRESENT | PAGE_WRITE);
    }

    // Replace PDE: point to new PT, remove PAGE_SIZE_BIT
    pd.add(pd_idx).write(pt as u64 | PAGE_PRESENT | PAGE_WRITE | (pde & PAGE_USER));
    pt
}

/// Mark a physical address range as user-accessible (PRESENT | WRITE | USER).
/// Splits 2MB large pages into 4KB pages as needed.
pub fn map_user(addr: u64, size: u64) {
    let pml4 = *PML4.lock();
    assert!(!pml4.is_null(), "paging: not initialized");

    let start = addr & !0xFFF;
    let end = (addr + size + 0xFFF) & !0xFFF;
    let mut cur = start;

    while cur < end {
        let pml4_idx = ((cur >> 39) & 0x1FF) as usize;
        let pdpt_idx = ((cur >> 30) & 0x1FF) as usize;
        let pd_idx = ((cur >> 21) & 0x1FF) as usize;
        let pt_idx = ((cur >> 12) & 0x1FF) as usize;

        unsafe {
            // Walk to PD, propagating USER on intermediate entries
            let user_flags = PAGE_PRESENT | PAGE_WRITE | PAGE_USER;
            let pdpt = get_or_create(pml4, pml4_idx, user_flags);
            let pd = get_or_create(pdpt, pdpt_idx, user_flags);

            // Check if PDE is a 2MB large page — if so, split it
            let pde = pd.add(pd_idx).read();
            let pt = if pde & PAGE_PRESENT != 0 && pde & PAGE_SIZE_BIT != 0 {
                split_2m(pd, pd_idx)
            } else if pde & PAGE_PRESENT != 0 {
                // Already a PT pointer, just add USER flag to PDE
                let updated = pde | PAGE_USER;
                if updated != pde {
                    pd.add(pd_idx).write(updated);
                }
                (pde & ADDR_MASK) as *mut u64
            } else {
                // No mapping — create PT (shouldn't normally happen with init)
                get_or_create(pd, pd_idx, user_flags)
            };

            // Set USER on the 4KB PTE
            let pte = pt.add(pt_idx).read();
            pt.add(pt_idx).write(pte | PAGE_USER);
        }

        cur += PAGE_4K;
    }

    cpu::flush_tlb();
    apic::tlb_shootdown();
}

/// Identity-map an MMIO region as kernel-only using 2MB large pages.
/// Call this before accessing PCI BARs that lie outside the initial mapping.
pub fn map_kernel(addr: u64, size: u64) {
    let pml4 = *PML4.lock();
    assert!(!pml4.is_null(), "paging: not initialized");

    let start = addr & !(PAGE_2M - 1); // round down to 2MB
    let end = (addr + size + PAGE_2M - 1) & !(PAGE_2M - 1); // round up
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

/// Remove user-accessible flag from a physical address range.
pub fn unmap_user(addr: u64, size: u64) {
    let pml4 = *PML4.lock();
    assert!(!pml4.is_null(), "paging: not initialized");

    let start = addr & !0xFFF;
    let end = (addr + size + 0xFFF) & !0xFFF;
    let mut cur = start;

    while cur < end {
        let pml4_idx = ((cur >> 39) & 0x1FF) as usize;
        let pdpt_idx = ((cur >> 30) & 0x1FF) as usize;
        let pd_idx = ((cur >> 21) & 0x1FF) as usize;
        let pt_idx = ((cur >> 12) & 0x1FF) as usize;

        unsafe {
            let pml4e = pml4.add(pml4_idx).read();
            if pml4e & PAGE_PRESENT == 0 { cur += PAGE_4K; continue; }
            let pdpt = (pml4e & ADDR_MASK) as *mut u64;

            let pdpte = pdpt.add(pdpt_idx).read();
            if pdpte & PAGE_PRESENT == 0 { cur += PAGE_4K; continue; }
            let pd = (pdpte & ADDR_MASK) as *mut u64;

            let pde = pd.add(pd_idx).read();
            if pde & PAGE_PRESENT == 0 || pde & PAGE_SIZE_BIT != 0 {
                // Not split into 4KB pages or not present — skip
                cur += PAGE_4K;
                continue;
            }
            let pt = (pde & ADDR_MASK) as *mut u64;

            let pte = pt.add(pt_idx).read();
            pt.add(pt_idx).write(pte & !PAGE_USER);
        }

        cur += PAGE_4K;
    }

    cpu::flush_tlb();
    apic::tlb_shootdown();
}
