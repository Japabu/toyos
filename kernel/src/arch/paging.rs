use core::alloc::Layout;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use alloc::alloc::{alloc_zeroed, dealloc};

use super::{apic, cpu};
use crate::{MemoryMapEntry, PhysAddr, UserAddr, PHYS_OFFSET};

const PAGE_PRESENT: u64 = 1 << 0;
const PAGE_WRITE: u64 = 1 << 1;
const PAGE_USER: u64 = 1 << 2;
const PAGE_SIZE_BIT: u64 = 1 << 7; // 2MB/1GB large page
const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

/// Convert a physical address from a PTE to a dereferenceable kernel pointer.
#[inline]
fn phys_to_ptr(phys: u64) -> *mut u64 {
    (phys + PHYS_OFFSET) as *mut u64
}

/// Convert a kernel virtual pointer to the physical address for a PTE.
#[inline]
fn ptr_to_phys(ptr: *mut u64) -> u64 {
    ptr as u64 - PHYS_OFFSET
}

const PAGE_4K: u64 = 4096;
pub const PAGE_2M: u64 = 2 * 1024 * 1024;

/// Round `size` up to the next 2MB boundary.
pub const fn align_2m(size: usize) -> usize {
    (size + PAGE_2M as usize - 1) & !(PAGE_2M as usize - 1)
}

/// Kernel PML4 template (virtual pointer). Per-process PML4s share kernel entries.
static KERNEL_PML4: AtomicPtr<u64> = AtomicPtr::new(core::ptr::null_mut());

/// How many PML4 entries the kernel direct map uses (starting at PML4[256]).
static KERNEL_PML4_ENTRIES: AtomicUsize = AtomicUsize::new(0);

/// Whether addr is in the kernel's high-half direct map.
pub fn is_kernel_addr(addr: u64) -> bool {
    addr >= PHYS_OFFSET
}

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

/// Build kernel page tables: map all physical memory in the high half
/// (PML4[256+]) using 2MB large pages. Also creates a temporary identity
/// map in PML4[0..N] for the boot transition and SMP trampoline.
pub fn init(memory_map: &[MemoryMapEntry]) {
    let mut max_addr: u64 = 4 * 1024 * 1024 * 1024; // 4GB minimum for MMIO
    for entry in memory_map {
        if entry.end > max_addr {
            max_addr = entry.end;
        }
    }
    max_addr = (max_addr + PAGE_2M - 1) & !(PAGE_2M - 1);

    let pml4 = alloc_page();

    // High-half direct map: physical addr P → virtual addr PHYS_OFFSET + P
    // PML4 index = 256 + (P >> 39)
    let mut addr: u64 = 0;
    while addr < max_addr {
        let virt = PHYS_OFFSET + addr;
        let pml4_idx = ((virt >> 39) & 0x1FF) as usize;
        let pdpt_idx = ((virt >> 30) & 0x1FF) as usize;
        let pd_idx = ((virt >> 21) & 0x1FF) as usize;

        let pdpt = unsafe { get_or_create(pml4, pml4_idx, PAGE_PRESENT | PAGE_WRITE) };
        let pd = unsafe { get_or_create(pdpt, pdpt_idx, PAGE_PRESENT | PAGE_WRITE) };
        unsafe {
            pd.add(pd_idx).write(addr | PAGE_PRESENT | PAGE_WRITE | PAGE_SIZE_BIT);
        }

        addr += PAGE_2M;
    }

    // Temporary identity map: physical P → virtual P (PML4[0..N])
    // Needed for: boot transition, SMP trampoline at 0x8000, serial I/O.
    // Will be dropped after boot_aps() completes.
    addr = 0;
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
    KERNEL_PML4.store(pml4, Ordering::Release);
    KERNEL_PML4_ENTRIES.store(entries, Ordering::Release);
    unsafe { cpu::write_cr3(PhysAddr::from_ptr(pml4)); }
}

/// Get or create a next-level page table at the given index.
/// `table` is a virtual pointer. PTEs store physical addresses.
unsafe fn get_or_create(table: *mut u64, index: usize, flags: u64) -> *mut u64 {
    let entry = table.add(index).read();
    if entry & PAGE_PRESENT != 0 {
        let updated = entry | (flags & (PAGE_PRESENT | PAGE_WRITE | PAGE_USER));
        if updated != entry {
            table.add(index).write(updated);
        }
        phys_to_ptr(entry & ADDR_MASK)
    } else {
        let new_table = alloc_page(); // returns virtual pointer
        table.add(index).write(ptr_to_phys(new_table) | flags);
        new_table
    }
}

/// Physical address of the kernel PML4 template. Used for idle/exit CR3.
pub fn kernel_cr3() -> PhysAddr {
    PhysAddr::from_ptr(KERNEL_PML4.load(Ordering::Acquire))
}

/// Create a per-process PML4.
///
/// - PML4[0..255]: empty (user mappings added on demand by map_user_in)
/// - PML4[256..511]: shallow-copy kernel high-half entries (shared, no USER bit)
///
/// The identity map entries (PML4[0..N]) are also copied for now so kernel code
/// running at identity-mapped addresses during syscalls still works. These will
/// be removed once the full higher-half migration is complete.
pub fn create_user_pml4() -> PhysAddr {
    let kernel_pml4 = KERNEL_PML4.load(Ordering::Acquire);
    assert!(!kernel_pml4.is_null(), "paging: not initialized");

    let pml4 = alloc_page();

    // Copy ALL kernel PML4 entries (both identity map and high-half).
    // Identity entries (0..N) are deep-cloned so map_user_in can set USER independently.
    // High-half entries (256+) are shallow-copied (shared, never get USER).
    for pml4_idx in 0..512 {
        let pml4e = unsafe { kernel_pml4.add(pml4_idx).read() };
        if pml4e & PAGE_PRESENT == 0 {
            continue;
        }

        if pml4_idx >= 256 {
            // High-half kernel entries: shallow copy (shared page tables)
            unsafe { pml4.add(pml4_idx).write(pml4e); }
        } else {
            // Identity map entries: deep clone PDPT→PD chain
            let kernel_pdpt = phys_to_ptr(pml4e & ADDR_MASK);
            let pdpt = alloc_page();

            for pdpt_idx in 0..512 {
                let pdpte = unsafe { kernel_pdpt.add(pdpt_idx).read() };
                if pdpte & PAGE_PRESENT == 0 {
                    continue;
                }
                let kernel_pd = phys_to_ptr(pdpte & ADDR_MASK);
                let pd = alloc_page();
                unsafe { core::ptr::copy_nonoverlapping(kernel_pd, pd, 512); }
                unsafe { pdpt.add(pdpt_idx).write(ptr_to_phys(pd) | (pdpte & !ADDR_MASK)); }
            }

            unsafe { pml4.add(pml4_idx).write(ptr_to_phys(pdpt) | (pml4e & !ADDR_MASK)); }
        }
    }

    PhysAddr::from_ptr(pml4)
}

/// Free a per-process PML4 and all its cloned PDPT/PD/PT tables.
/// Does NOT free the underlying physical pages (2MB or 4KB data pages).
pub fn free_user_page_tables(pml4: *mut u64) {
    // Only free PML4[0..255] — these are per-process entries (identity map clones + user mappings).
    // PML4[256+] entries are shared kernel entries — do NOT free them.
    for pml4_idx in 0..256 {
        let pml4e = unsafe { pml4.add(pml4_idx).read() };
        if pml4e & PAGE_PRESENT == 0 {
            continue;
        }
        let pdpt = phys_to_ptr(pml4e & ADDR_MASK);

        for pdpt_idx in 0..512 {
            let pdpte = unsafe { pdpt.add(pdpt_idx).read() };
            if pdpte & PAGE_PRESENT == 0 {
                continue;
            }
            let pd = phys_to_ptr(pdpte & ADDR_MASK);

            // Free any 4KB page tables under this PD
            for pd_idx in 0..512 {
                let pde = unsafe { pd.add(pd_idx).read() };
                if pde & PAGE_PRESENT != 0 && pde & PAGE_SIZE_BIT == 0 {
                    free_page(phys_to_ptr(pde & ADDR_MASK));
                }
            }

            free_page(pd);
        }

        free_page(pdpt);
    }

    free_page(pml4);
}

/// Map physical memory as user-accessible in a specific PML4.
/// For 2MB-aligned regions that fill entire 2MB pages, uses 2MB page granularity.
/// For sub-2MB or non-2MB-aligned regions in the identity-mapped range, uses 4KB
/// granularity to avoid USER bit contamination of neighboring kernel pages.
pub fn map_user_in(pml4: *mut u64, addr: PhysAddr, size: u64) {
    let raw = addr.raw();
    let entries = KERNEL_PML4_ENTRIES.load(Ordering::Acquire);
    let pml4_idx = ((raw >> 39) & 0x1FF) as usize;

    // Identity-mapped range: use 4KB mapping if not perfectly 2MB-aligned and 2MB-sized,
    // to prevent USER contamination of neighboring kernel pages in the same 2MB range.
    let is_identity = pml4_idx < entries;
    let is_2m_aligned = (raw & (PAGE_2M - 1)) == 0 && (size & (PAGE_2M - 1)) == 0;

    if is_identity && !is_2m_aligned {
        // 4KB granularity: only set USER on the specific pages
        let start = raw & !(PAGE_4K - 1);
        let end = (raw + size + PAGE_4K - 1) & !(PAGE_4K - 1);
        let mut cur = start;
        while cur < end {
            map_4k_in(pml4, UserAddr::new(cur), PhysAddr::new(cur), true);
            cur += PAGE_4K;
        }
        return;
    }

    let start = raw & !(PAGE_2M - 1);
    let end = (raw + size + PAGE_2M - 1) & !(PAGE_2M - 1);
    let mut cur = start;
    while cur < end {
        map_user_2m(pml4, cur);
        cur += PAGE_2M;
    }
}

/// Set USER on a single 2MB PDE and its parent entries.
fn map_user_2m(pml4: *mut u64, addr: u64) {
    let pml4_idx = ((addr >> 39) & 0x1FF) as usize;
    let pdpt_idx = ((addr >> 30) & 0x1FF) as usize;
    let pd_idx = ((addr >> 21) & 0x1FF) as usize;
    unsafe {
        let pml4e = pml4.add(pml4_idx).read();
        assert!(pml4e & PAGE_PRESENT != 0, "map_user: PML4 not present for {:#x}", addr);
        pml4.add(pml4_idx).write(pml4e | PAGE_USER);

        let pdpt = phys_to_ptr(pml4e & ADDR_MASK);
        let pdpte = pdpt.add(pdpt_idx).read();
        assert!(pdpte & PAGE_PRESENT != 0, "map_user: PDPT not present for {:#x}", addr);
        pdpt.add(pdpt_idx).write(pdpte | PAGE_USER);

        let pd = phys_to_ptr(pdpte & ADDR_MASK);
        let pde = pd.add(pd_idx).read();
        assert!(pde & PAGE_PRESENT != 0, "map_user: PD not present for {:#x}", addr);
        assert!(pde & PAGE_SIZE_BIT != 0, "map_user: expected 2MB page at {:#x}", addr);
        pd.add(pd_idx).write(pde | PAGE_USER);
    }
}

/// Set USER bit and clear WRITE bit on pages in a specific PML4.
/// Uses 4KB granularity for identity-mapped addresses, 2MB for user virtual.
pub fn map_user_readonly_in(pml4: *mut u64, addr: PhysAddr, size: u64) {
    let entries = KERNEL_PML4_ENTRIES.load(Ordering::Acquire);
    let raw = addr.raw();
    let pml4_idx = ((raw >> 39) & 0x1FF) as usize;

    if pml4_idx < entries {
        // Identity-mapped: 4KB granularity, read-only
        let start = raw & !(PAGE_4K - 1);
        let end = (raw + size + PAGE_4K - 1) & !(PAGE_4K - 1);
        let mut cur = start;
        while cur < end {
            map_4k_in(pml4, UserAddr::new(cur), PhysAddr::new(cur), false);
            cur += PAGE_4K;
        }
    } else {
        // User virtual: 2MB granularity
        let start = raw & !(PAGE_2M - 1);
        let end = (raw + size + PAGE_2M - 1) & !(PAGE_2M - 1);
        let mut cur = start;
        while cur < end {
            let pdpt_idx = ((cur >> 30) & 0x1FF) as usize;
            let pd_idx = ((cur >> 21) & 0x1FF) as usize;
            unsafe {
                let pml4e = pml4.add(pml4_idx).read();
                assert!(pml4e & PAGE_PRESENT != 0, "map_user_ro: PML4 not present for {:#x}", cur);
                pml4.add(pml4_idx).write(pml4e | PAGE_USER);
                let pdpt = phys_to_ptr(pml4e & ADDR_MASK);
                let pdpte = pdpt.add(pdpt_idx).read();
                assert!(pdpte & PAGE_PRESENT != 0, "map_user_ro: PDPT not present for {:#x}", cur);
                pdpt.add(pdpt_idx).write(pdpte | PAGE_USER);
                let pd = phys_to_ptr(pdpte & ADDR_MASK);
                let pde = pd.add(pd_idx).read();
                assert!(pde & PAGE_PRESENT != 0, "map_user_ro: PD not present for {:#x}", cur);
                assert!(pde & PAGE_SIZE_BIT != 0, "map_user_ro: expected 2MB page at {:#x}", cur);
                pd.add(pd_idx).write((pde | PAGE_USER) & !PAGE_WRITE);
            }
            cur += PAGE_2M;
        }
    }
}

/// Set USER bit on 2MB pages in the current process's page tables (via CR3).
pub fn map_user(addr: PhysAddr, size: u64) {
    let pml4 = cpu::read_cr3().as_mut_ptr();
    map_user_in(pml4, addr, size);
    cpu::flush_tlb();
    apic::tlb_shootdown();
}

/// Remap a 2MB-aligned virtual address to point to different physical memory
/// in a specific page table. No TLB flush — caller is responsible.
/// Sets the PDE to `phys_addr` with PRESENT|WRITE|USER|PS bits.
pub fn remap_user_2m_in(pml4: *mut u64, virt_addr: UserAddr, phys_addr: PhysAddr) -> bool {
    let va = virt_addr.raw();
    if va & (PAGE_2M - 1) != 0 || phys_addr.raw() & (PAGE_2M - 1) != 0 {
        return false;
    }

    let pml4_idx = ((va >> 39) & 0x1FF) as usize;
    let pdpt_idx = ((va >> 30) & 0x1FF) as usize;
    let pd_idx = ((va >> 21) & 0x1FF) as usize;

    unsafe {
        let pml4e = pml4.add(pml4_idx).read();
        if pml4e & PAGE_PRESENT == 0 { return false; }
        pml4.add(pml4_idx).write(pml4e | PAGE_USER);

        let pdpt = phys_to_ptr(pml4e & ADDR_MASK);
        let pdpte = pdpt.add(pdpt_idx).read();
        if pdpte & PAGE_PRESENT == 0 { return false; }
        pdpt.add(pdpt_idx).write(pdpte | PAGE_USER);

        let pd = phys_to_ptr(pdpte & ADDR_MASK);
        let old_pde = pd.add(pd_idx).read();
        // If the PDE was split into a 4KB PT, free the orphaned PT page.
        if old_pde & PAGE_PRESENT != 0 && old_pde & PAGE_SIZE_BIT == 0 {
            free_page(phys_to_ptr(old_pde & ADDR_MASK));
        }
        pd.add(pd_idx).write(phys_addr.raw() | PAGE_PRESENT | PAGE_WRITE | PAGE_USER | PAGE_SIZE_BIT);
    }

    true
}

/// Restore a 2MB PDE to its identity-mapped value (phys == virt, no USER bit).
pub fn restore_identity_2m(pml4: *mut u64, virt_addr: UserAddr) {
    let va = virt_addr.raw();
    let pml4_idx = ((va >> 39) & 0x1FF) as usize;
    let pdpt_idx = ((va >> 30) & 0x1FF) as usize;
    let pd_idx = ((va >> 21) & 0x1FF) as usize;

    unsafe {
        let pml4e = pml4.add(pml4_idx).read();
        if pml4e & PAGE_PRESENT == 0 { return; }
        let pdpt = phys_to_ptr(pml4e & ADDR_MASK);
        let pdpte = pdpt.add(pdpt_idx).read();
        if pdpte & PAGE_PRESENT == 0 { return; }
        let pd = phys_to_ptr(pdpte & ADDR_MASK);
        pd.add(pd_idx).write(va | PAGE_PRESENT | PAGE_WRITE | PAGE_SIZE_BIT);
    }

    cpu::flush_tlb();
    apic::tlb_shootdown();
}

/// Clear USER bit on pages in a specific PML4.
/// Handles both 2MB large pages and 4KB page tables (from split pages).
/// For sub-2MB regions in split PDs, only clears the specific PTEs.
pub fn unmap_user(pml4: *mut u64, addr: PhysAddr, size: u64) {
    let raw = addr.raw();

    // For sub-2MB, non-2MB-aligned regions, use 4KB granularity
    let is_2m_aligned = (raw & (PAGE_2M - 1)) == 0 && (size & (PAGE_2M - 1)) == 0;
    if !is_2m_aligned {
        let start = raw & !(PAGE_4K - 1);
        let end = (raw + size + PAGE_4K - 1) & !(PAGE_4K - 1);
        let mut cur = start;
        while cur < end {
            let pml4_idx = ((cur >> 39) & 0x1FF) as usize;
            let pdpt_idx = ((cur >> 30) & 0x1FF) as usize;
            let pd_idx = ((cur >> 21) & 0x1FF) as usize;
            let pt_idx = ((cur >> 12) & 0x1FF) as usize;
            unsafe {
                let pml4e = pml4.add(pml4_idx).read();
                if pml4e & PAGE_PRESENT == 0 { cur += PAGE_4K; continue; }
                let pdpt = phys_to_ptr(pml4e & ADDR_MASK);
                let pdpte = pdpt.add(pdpt_idx).read();
                if pdpte & PAGE_PRESENT == 0 { cur += PAGE_4K; continue; }
                let pd = phys_to_ptr(pdpte & ADDR_MASK);
                let pde = pd.add(pd_idx).read();
                if pde & PAGE_PRESENT == 0 { cur += PAGE_4K; continue; }
                if pde & PAGE_SIZE_BIT != 0 {
                    // 2MB large page: can't clear USER on individual pages
                    // This shouldn't happen for sub-2MB mappings that used 4KB map_user_in
                    cur += PAGE_4K;
                    continue;
                }
                let pt = phys_to_ptr(pde & ADDR_MASK);
                let pte = pt.add(pt_idx).read();
                if pte & PAGE_USER != 0 {
                    pt.add(pt_idx).write(pte & !PAGE_USER);
                }
            }
            cur += PAGE_4K;
        }
        return;
    }

    let start = raw & !(PAGE_2M - 1);
    let end = (raw + size + PAGE_2M - 1) & !(PAGE_2M - 1);
    let mut cur = start;
    while cur < end {
        let pml4_idx = ((cur >> 39) & 0x1FF) as usize;
        let pdpt_idx = ((cur >> 30) & 0x1FF) as usize;
        let pd_idx = ((cur >> 21) & 0x1FF) as usize;
        unsafe {
            let pml4e = pml4.add(pml4_idx).read();
            if pml4e & PAGE_PRESENT == 0 { cur += PAGE_2M; continue; }
            let pdpt = phys_to_ptr(pml4e & ADDR_MASK);
            let pdpte = pdpt.add(pdpt_idx).read();
            if pdpte & PAGE_PRESENT == 0 { cur += PAGE_2M; continue; }
            let pd = phys_to_ptr(pdpte & ADDR_MASK);
            let pde = pd.add(pd_idx).read();
            if pde & PAGE_PRESENT == 0 { cur += PAGE_2M; continue; }
            if pde & PAGE_SIZE_BIT != 0 {
                // 2MB large page: clear USER on the PDE
                pd.add(pd_idx).write(pde & !PAGE_USER);
            } else {
                // Split into 4KB PT: clear USER on all PTEs
                let pt = phys_to_ptr(pde & ADDR_MASK);
                for i in 0..512usize {
                    let pte = pt.add(i).read();
                    if pte & PAGE_USER != 0 {
                        pt.add(i).write(pte & !PAGE_USER);
                    }
                }
            }
        }
        cur += PAGE_2M;
    }
}

/// Identity-map an MMIO region as kernel-only using 2MB large pages.
/// Only call during boot before any processes exist.
pub fn map_kernel(addr: PhysAddr, size: u64) {
    let pml4 = KERNEL_PML4.load(Ordering::Acquire);
    assert!(!pml4.is_null(), "paging: not initialized");

    let raw = addr.raw();
    let start = raw & !(PAGE_2M - 1);
    let end = (raw + size + PAGE_2M - 1) & !(PAGE_2M - 1);
    let mut cur = start;

    while cur < end {
        // Map in the high-half direct map: virtual = PHYS_OFFSET + physical
        let virt = PHYS_OFFSET + cur;
        let pml4_idx = ((virt >> 39) & 0x1FF) as usize;
        let pdpt_idx = ((virt >> 30) & 0x1FF) as usize;
        let pd_idx = ((virt >> 21) & 0x1FF) as usize;

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

/// Map a 4KB page at `virt_addr` pointing to `phys_addr` in the given PML4.
/// Creates intermediate page table levels (PDPT, PD, PT) as needed.
/// If the PD entry is a 2MB large page, it is split into 512 identity-mapped 4KB PTEs first.
pub fn map_4k_in(pml4: *mut u64, virt_addr: UserAddr, phys_addr: PhysAddr, writable: bool) {
    let va = virt_addr.raw();
    let pml4_idx = ((va >> 39) & 0x1FF) as usize;
    let pdpt_idx = ((va >> 30) & 0x1FF) as usize;
    let pd_idx = ((va >> 21) & 0x1FF) as usize;
    let pt_idx = ((va >> 12) & 0x1FF) as usize;

    let user_flags = PAGE_PRESENT | PAGE_WRITE | PAGE_USER;

    unsafe {
        let pdpt = get_or_create(pml4, pml4_idx, user_flags);
        let pd = get_or_create(pdpt, pdpt_idx, user_flags);

        let pde = pd.add(pd_idx).read();
        let pt = if pde & PAGE_PRESENT != 0 && pde & PAGE_SIZE_BIT != 0 {
            // Split 2MB large page into 512 identity-mapped 4KB PTEs
            let base_phys = pde & ADDR_MASK;
            let pt = alloc_page();
            for i in 0..512u64 {
                pt.add(i as usize).write(
                    (base_phys + i * PAGE_4K) | PAGE_PRESENT | PAGE_WRITE
                    | (pde & PAGE_USER) // preserve USER bit from original PDE
                );
            }
            pd.add(pd_idx).write(ptr_to_phys(pt) | user_flags);
            pt
        } else if pde & PAGE_PRESENT != 0 {
            phys_to_ptr(pde & ADDR_MASK)
        } else {
            let pt = alloc_page();
            pd.add(pd_idx).write(ptr_to_phys(pt) | user_flags);
            pt
        };

        let mut flags = PAGE_PRESENT | PAGE_USER;
        if writable { flags |= PAGE_WRITE; }
        pt.add(pt_idx).write((phys_addr.raw() & ADDR_MASK) | flags);
    }
}

/// Translate a virtual address to its physical address by walking the page tables.
/// Returns `None` if any level is not present.
pub fn virt_to_phys(pml4: *const u64, virt_addr: UserAddr) -> Option<PhysAddr> {
    let va = virt_addr.raw();
    let pml4_idx = ((va >> 39) & 0x1FF) as usize;
    let pdpt_idx = ((va >> 30) & 0x1FF) as usize;
    let pd_idx = ((va >> 21) & 0x1FF) as usize;

    unsafe {
        let pml4e = pml4.add(pml4_idx).read();
        if pml4e & PAGE_PRESENT == 0 { return None; }
        let pdpt = phys_to_ptr(pml4e & ADDR_MASK) as *const u64;
        let pdpte = pdpt.add(pdpt_idx).read();
        if pdpte & PAGE_PRESENT == 0 { return None; }
        let pd = phys_to_ptr(pdpte & ADDR_MASK) as *const u64;
        let pde = pd.add(pd_idx).read();
        if pde & PAGE_PRESENT == 0 { return None; }

        if pde & PAGE_SIZE_BIT != 0 {
            // 2MB large page
            Some(PhysAddr::new((pde & ADDR_MASK) + (va & (PAGE_2M - 1))))
        } else {
            // 4KB page table
            let pt = phys_to_ptr(pde & ADDR_MASK) as *const u64;
            let pt_idx = ((va >> 12) & 0x1FF) as usize;
            let pte = pt.add(pt_idx).read();
            if pte & PAGE_PRESENT == 0 { return None; }
            Some(PhysAddr::new((pte & ADDR_MASK) + (va & 0xFFF)))
        }
    }
}

/// Return the address of the PTE for a 4KB-mapped virtual address.
/// Returns None if any level is missing or if the PD entry is a 2MB page.
#[allow(dead_code)]
pub fn pte_addr(pml4: *const u64, virt_addr: u64) -> Option<u64> {
    let pml4_idx = ((virt_addr >> 39) & 0x1FF) as usize;
    let pdpt_idx = ((virt_addr >> 30) & 0x1FF) as usize;
    let pd_idx = ((virt_addr >> 21) & 0x1FF) as usize;
    let pt_idx = ((virt_addr >> 12) & 0x1FF) as usize;

    unsafe {
        let pml4e = pml4.add(pml4_idx).read();
        if pml4e & PAGE_PRESENT == 0 { return None; }
        let pdpt = phys_to_ptr(pml4e & ADDR_MASK) as *const u64;
        let pdpte = pdpt.add(pdpt_idx).read();
        if pdpte & PAGE_PRESENT == 0 { return None; }
        let pd = phys_to_ptr(pdpte & ADDR_MASK) as *const u64;
        let pde = pd.add(pd_idx).read();
        if pde & PAGE_PRESENT == 0 || pde & PAGE_SIZE_BIT != 0 { return None; }
        let pt = phys_to_ptr(pde & ADDR_MASK) as *const u64;
        Some(pt.add(pt_idx) as u64)
    }
}

fn has(entry: u64, flag: u64) -> u8 {
    if entry & flag != 0 { 1 } else { 0 }
}

/// Dump page table entries for an address. Lock-free for crash safety.
pub fn debug_page_walk(addr: u64) {
    use crate::log;

    let pml4: *const u64 = cpu::read_cr3().as_ptr();
    let pml4_idx = ((addr >> 39) & 0x1FF) as usize;
    let pdpt_idx = ((addr >> 30) & 0x1FF) as usize;
    let pd_idx = ((addr >> 21) & 0x1FF) as usize;
    let pt_idx = ((addr >> 12) & 0x1FF) as usize;

    log!("  Page walk for {:#x} [PML4={:#x} PML4[{}] PDPT[{}] PD[{}] PT[{}]]:",
        addr, pml4 as u64, pml4_idx, pdpt_idx, pd_idx, pt_idx);

    unsafe {
        let pml4e = pml4.add(pml4_idx).read_volatile();
        log!("    PML4E: {:#018x} P={} W={} U={}", pml4e,
            has(pml4e, PAGE_PRESENT), has(pml4e, PAGE_WRITE), has(pml4e, PAGE_USER));
        if pml4e & PAGE_PRESENT == 0 { return; }

        let pdpt = phys_to_ptr(pml4e & ADDR_MASK) as *const u64;
        let pdpte = pdpt.add(pdpt_idx).read_volatile();
        log!("    PDPTE: {:#018x} P={} W={} U={}", pdpte,
            has(pdpte, PAGE_PRESENT), has(pdpte, PAGE_WRITE), has(pdpte, PAGE_USER));
        if pdpte & PAGE_PRESENT == 0 { return; }

        let pd = phys_to_ptr(pdpte & ADDR_MASK) as *const u64;
        let pde = pd.add(pd_idx).read_volatile();
        log!("    PDE:   {:#018x} P={} W={} U={} PS={}", pde,
            has(pde, PAGE_PRESENT), has(pde, PAGE_WRITE), has(pde, PAGE_USER), has(pde, PAGE_SIZE_BIT));
        if pde & PAGE_PRESENT == 0 { return; }
        if pde & PAGE_SIZE_BIT != 0 {
            log!("    -> 2MB large page at {:#x}", pde & ADDR_MASK);
            return;
        }

        let pt = phys_to_ptr(pde & ADDR_MASK) as *const u64;
        let pte = pt.add(pt_idx).read_volatile();
        log!("    PTE:   {:#018x} P={} W={} U={}", pte,
            has(pte, PAGE_PRESENT), has(pte, PAGE_WRITE), has(pte, PAGE_USER));
        if pte & PAGE_PRESENT == 0 { return; }
        log!("    -> 4KB page at {:#x}", pte & ADDR_MASK);
    }
}
