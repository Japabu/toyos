use core::alloc::Layout;
use core::sync::atomic::{AtomicPtr, Ordering};

use alloc::alloc::{alloc_zeroed, dealloc};

use super::{apic, cpu};
use crate::{MemoryMapEntry, PhysAddr, UserAddr, PHYS_OFFSET};

const PAGE_PRESENT: u64 = 1 << 0;
const PAGE_WRITE: u64 = 1 << 1;
const PAGE_USER: u64 = 1 << 2;
const PAGE_SIZE_BIT: u64 = 1 << 7; // 2MB large page
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
const MIN_PHYS_MAP: u64 = 4 * 1024 * 1024 * 1024; // 4GB minimum (covers MMIO regions)

/// Extract PML4, PDPT, and PD indices from a virtual address.
#[inline]
fn page_indices(addr: u64) -> (usize, usize, usize) {
    (
        ((addr >> 39) & 0x1FF) as usize,
        ((addr >> 30) & 0x1FF) as usize,
        ((addr >> 21) & 0x1FF) as usize,
    )
}

/// Round `size` up to the next 2MB boundary.
pub const fn align_2m(size: usize) -> usize {
    (size + PAGE_2M as usize - 1) & !(PAGE_2M as usize - 1)
}

/// Kernel PML4 template (virtual pointer). Per-process PML4s share kernel entries.
/// PML4[256+] = high-half direct map. PML4[0..255] = empty (no identity map).
static KERNEL_PML4: AtomicPtr<u64> = AtomicPtr::new(core::ptr::null_mut());

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
/// (PML4[256+]) using 2MB large pages. No identity map — PML4[0..255] is empty.
pub fn init(memory_map: &[MemoryMapEntry]) {
    let mut max_addr: u64 = MIN_PHYS_MAP;
    for entry in memory_map {
        if entry.end > max_addr {
            max_addr = entry.end;
        }
    }
    max_addr = (max_addr + PAGE_2M - 1) & !(PAGE_2M - 1);

    let pml4 = alloc_page();

    // High-half direct map only: physical addr P → virtual addr PHYS_OFFSET + P
    let mut addr: u64 = 0;
    while addr < max_addr {
        let (pml4_idx, pdpt_idx, pd_idx) = page_indices(PHYS_OFFSET + addr);
        let pdpt = unsafe { get_or_create(pml4, pml4_idx, PAGE_PRESENT | PAGE_WRITE) };
        let pd = unsafe { get_or_create(pdpt, pdpt_idx, PAGE_PRESENT | PAGE_WRITE) };
        unsafe {
            pd.add(pd_idx).write(addr | PAGE_PRESENT | PAGE_WRITE | PAGE_SIZE_BIT);
        }

        addr += PAGE_2M;
    }

    // Log the PD page address for PDPT[0] (first 1GB of direct map) for debugging
    let pdpt0 = unsafe { get_or_create(pml4, 256, PAGE_PRESENT | PAGE_WRITE) };
    let pd0 = unsafe { get_or_create(pdpt0, 0, PAGE_PRESENT | PAGE_WRITE) };
    crate::log!("paging: PD for first 1GB direct map at virt={:#x} phys={:#x} early_buf={:#x}..{:#x}",
        pd0 as u64, ptr_to_phys(pd0),
        crate::allocator::early_buf_range().0,
        crate::allocator::early_buf_range().1);
    let pde228 = unsafe { pd0.add(228).read() };
    crate::log!("paging: PDE[228]={:#018x} (should be 0x1c8000e3)", pde228);

    KERNEL_PML4.store(pml4, Ordering::Release);
    unsafe { cpu::write_cr3(PhysAddr::from_ptr(pml4)); }
}

/// DEBUG: verify a kernel direct-map PDE hasn't been corrupted with a virtual address.
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
/// - PML4[0..255]: empty (user mappings created on demand by map_user_in)
/// - PML4[256..511]: shallow-copy kernel high-half entries (shared, no USER bit)
pub fn create_user_pml4() -> PhysAddr {
    let kernel_pml4 = KERNEL_PML4.load(Ordering::Acquire);
    assert!(!kernel_pml4.is_null(), "paging: not initialized");

    let pml4 = alloc_page();

    // Shallow-copy kernel high-half entries (PML4[256+]).
    // These are shared across all processes and never get USER bit.
    for pml4_idx in 256..512 {
        let pml4e = unsafe { kernel_pml4.add(pml4_idx).read() };
        if pml4e & PAGE_PRESENT != 0 {
            unsafe { pml4.add(pml4_idx).write(pml4e); }
        }
    }

    PhysAddr::from_ptr(pml4)
}

/// Free a per-process PML4 and all its user page table structures.
/// Only frees PML4[0..255] (user space). PML4[256+] are shared kernel entries.
/// Does NOT free the underlying physical data pages.
pub fn free_user_page_tables(pml4: *mut u64) {
    for pml4_idx in 0..256 {
        let pml4e = unsafe { pml4.add(pml4_idx).read() };
        if pml4e & PAGE_PRESENT == 0 { continue; }
        let pdpt = phys_to_ptr(pml4e & ADDR_MASK);

        for pdpt_idx in 0..512 {
            let pdpte = unsafe { pdpt.add(pdpt_idx).read() };
            if pdpte & PAGE_PRESENT == 0 { continue; }
            free_page(phys_to_ptr(pdpte & ADDR_MASK));
        }

        free_page(pdpt);
    }

    free_page(pml4);
}

/// Map physical memory as user-accessible (2MB pages) in a specific PML4.
/// Creates page table structures (PDPT, PD) on demand.
pub fn map_user_in(pml4: *mut u64, addr: PhysAddr, size: u64) {
    debug_assert!(addr.raw() < PHYS_OFFSET, "map_user_in: addr {:#x} looks like a virtual address", addr.raw());
    let raw = addr.raw();
    let start = raw & !(PAGE_2M - 1);
    let end = (raw + size + PAGE_2M - 1) & !(PAGE_2M - 1);
    let mut cur = start;
    while cur < end {
        let (pml4_idx, pdpt_idx, pd_idx) = page_indices(cur);
        unsafe {
            let pdpt = get_or_create(pml4, pml4_idx, PAGE_PRESENT | PAGE_WRITE | PAGE_USER);
            let pd = get_or_create(pdpt, pdpt_idx, PAGE_PRESENT | PAGE_WRITE | PAGE_USER);
            // DEBUG: check if this pd is the kernel's PD for first 1GB
            let kernel_pml4 = KERNEL_PML4.load(core::sync::atomic::Ordering::Acquire);
            if !kernel_pml4.is_null() {
                let k256 = kernel_pml4.add(256).read();
                if k256 & PAGE_PRESENT != 0 {
                    let k_pdpt = phys_to_ptr(k256 & ADDR_MASK);
                    let k_pdpte0 = k_pdpt.read();
                    if k_pdpte0 & PAGE_PRESENT != 0 {
                        let k_pd = phys_to_ptr(k_pdpte0 & ADDR_MASK);
                        if pd == k_pd {
                            panic!("map_user_in: pd == kernel PD! addr={:#x} pml4_idx={} pdpt_idx={} pd_idx={}",
                                cur, pml4_idx, pdpt_idx, pd_idx);
                        }
                    }
                }
            }
            pd.add(pd_idx).write(cur | PAGE_PRESENT | PAGE_WRITE | PAGE_USER | PAGE_SIZE_BIT);
        }
        cur += PAGE_2M;
    }
}

/// Map physical memory as user-accessible read-only (2MB pages) in a specific PML4.
pub fn map_user_readonly_in(pml4: *mut u64, addr: PhysAddr, size: u64) {
    debug_assert!(addr.raw() < PHYS_OFFSET, "map_user_readonly_in: addr {:#x} looks like a virtual address", addr.raw());
    let raw = addr.raw();
    let start = raw & !(PAGE_2M - 1);
    let end = (raw + size + PAGE_2M - 1) & !(PAGE_2M - 1);
    let mut cur = start;
    while cur < end {
        let (pml4_idx, pdpt_idx, pd_idx) = page_indices(cur);
        unsafe {
            let pdpt = get_or_create(pml4, pml4_idx, PAGE_PRESENT | PAGE_WRITE | PAGE_USER);
            let pd = get_or_create(pdpt, pdpt_idx, PAGE_PRESENT | PAGE_WRITE | PAGE_USER);
            pd.add(pd_idx).write(cur | PAGE_PRESENT | PAGE_USER | PAGE_SIZE_BIT);
        }
        cur += PAGE_2M;
    }
}

/// Map user-accessible 2MB pages in the current process's page tables (via CR3).
pub fn map_user(addr: PhysAddr, size: u64) {
    let pml4 = cpu::read_cr3().as_mut_ptr();
    map_user_in(pml4, addr, size);
    cpu::flush_tlb();
    apic::tlb_shootdown();
}

/// Map a 2MB page at `virt_addr` pointing to `phys_addr` in a user PML4.
/// Creates page table structures on demand. No TLB flush — caller is responsible.
pub fn remap_user_2m_in(pml4: *mut u64, virt_addr: UserAddr, phys_addr: PhysAddr) -> bool {
    remap_user_2m(pml4, virt_addr, phys_addr, true)
}

/// Map a 2MB page at `virt_addr` pointing to `phys_addr` with explicit write control.
pub fn remap_user_2m(pml4: *mut u64, virt_addr: UserAddr, phys_addr: PhysAddr, writable: bool) -> bool {
    let va = virt_addr.raw();
    if va & (PAGE_2M - 1) != 0 || phys_addr.raw() & (PAGE_2M - 1) != 0 {
        return false;
    }

    let (pml4_idx, pdpt_idx, pd_idx) = page_indices(va);
    let mut flags = PAGE_PRESENT | PAGE_USER | PAGE_SIZE_BIT;
    if writable { flags |= PAGE_WRITE; }
    unsafe {
        let pdpt = get_or_create(pml4, pml4_idx, PAGE_PRESENT | PAGE_WRITE | PAGE_USER);
        let pd = get_or_create(pdpt, pdpt_idx, PAGE_PRESENT | PAGE_WRITE | PAGE_USER);
        pd.add(pd_idx).write(phys_addr.raw() | flags);
    }

    true
}

/// Clear a 2MB user PDE (unmap the page).
pub fn clear_user_2m(pml4: *mut u64, virt_addr: UserAddr) {
    let (pml4_idx, pdpt_idx, pd_idx) = page_indices(virt_addr.raw());
    unsafe {
        let pml4e = pml4.add(pml4_idx).read();
        if pml4e & PAGE_PRESENT == 0 { return; }
        let pdpt = phys_to_ptr(pml4e & ADDR_MASK);
        let pdpte = pdpt.add(pdpt_idx).read();
        if pdpte & PAGE_PRESENT == 0 { return; }
        let pd = phys_to_ptr(pdpte & ADDR_MASK);
        pd.add(pd_idx).write(0);
    }

    cpu::flush_tlb();
    apic::tlb_shootdown();
}

/// Unmap user pages — zero the 2MB PDEs.
pub fn unmap_user(pml4: *mut u64, addr: PhysAddr, size: u64) {
    let raw = addr.raw();
    let start = raw & !(PAGE_2M - 1);
    let end = (raw + size + PAGE_2M - 1) & !(PAGE_2M - 1);
    let mut cur = start;
    while cur < end {
        let (pml4_idx, pdpt_idx, pd_idx) = page_indices(cur);
        unsafe {
            let pml4e = pml4.add(pml4_idx).read();
            if pml4e & PAGE_PRESENT == 0 { cur += PAGE_2M; continue; }
            let pdpt = phys_to_ptr(pml4e & ADDR_MASK);
            let pdpte = pdpt.add(pdpt_idx).read();
            if pdpte & PAGE_PRESENT == 0 { cur += PAGE_2M; continue; }
            let pd = phys_to_ptr(pdpte & ADDR_MASK);
            pd.add(pd_idx).write(0);
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
        let (pml4_idx, pdpt_idx, pd_idx) = page_indices(PHYS_OFFSET + cur);
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

/// Translate a virtual address to its physical address by walking the page tables.
/// All user mappings use 2MB pages.
pub fn virt_to_phys(pml4: *const u64, virt_addr: UserAddr) -> Option<PhysAddr> {
    let va = virt_addr.raw();
    let (pml4_idx, pdpt_idx, pd_idx) = page_indices(va);
    unsafe {
        let pml4e = pml4.add(pml4_idx).read();
        if pml4e & PAGE_PRESENT == 0 { return None; }
        let pdpt = phys_to_ptr(pml4e & ADDR_MASK) as *const u64;
        let pdpte = pdpt.add(pdpt_idx).read();
        if pdpte & PAGE_PRESENT == 0 { return None; }
        let pd = phys_to_ptr(pdpte & ADDR_MASK) as *const u64;
        let pde = pd.add(pd_idx).read();
        if pde & PAGE_PRESENT == 0 { return None; }
        Some(PhysAddr::new((pde & ADDR_MASK) + (va & (PAGE_2M - 1))))
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
