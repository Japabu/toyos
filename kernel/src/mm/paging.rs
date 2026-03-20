// Page tables and address spaces.
//
// The only code that writes page table entries. Manages the kernel direct map
// (all physical memory at PHYS_OFFSET) and per-process user address spaces.

use core::sync::atomic::{AtomicPtr, Ordering};

use super::pmm;
use super::{PHYS_OFFSET, PAGE_2M, UserAddr};
use super::pmm::PhysPage;
use crate::sync::Lock;
use crate::MemoryMapEntry;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const PAGE_PRESENT: u64 = 1 << 0;
const PAGE_WRITE: u64 = 1 << 1;
const PAGE_USER: u64 = 1 << 2;
const PAGE_SIZE_BIT: u64 = 1 << 7;
const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;
const ADDR_MASK_2M: u64 = 0x000F_FFFF_FFE0_0000;

const PAGE_4K: usize = 4096;
/// Minimum physical address space to map (covers typical MMIO regions).
const MIN_PHYS_MAP: u64 = 4 * 1024 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Page table page slab — 2MB pages carved into 4KB page table pages
// ---------------------------------------------------------------------------

struct PageTableSlab {
    current: *mut u8,
    offset: usize,
}

unsafe impl Send for PageTableSlab {}

impl PageTableSlab {
    const fn new() -> Self {
        Self { current: core::ptr::null_mut(), offset: PAGE_2M as usize }
    }

    fn alloc_4k(&mut self) -> *mut u64 {
        if self.offset >= PAGE_2M as usize {
            let page = pmm::alloc_page().expect("paging: out of memory for page tables");
            self.current = page.as_ptr();
            self.offset = 0;
            core::mem::forget(page); // page table pages are managed by address space lifetime
        }
        let ptr = unsafe { self.current.add(self.offset) } as *mut u64;
        self.offset += PAGE_4K;
        // page is already zeroed by pmm::alloc_page
        ptr
    }
}

static PT_SLAB: Lock<PageTableSlab> = Lock::new(PageTableSlab::new());

// ---------------------------------------------------------------------------
// PageTable — low-level page table access
// ---------------------------------------------------------------------------

/// A page table accessible via the kernel direct map.
/// Holds a kernel virtual pointer. All entry values are physical addresses.
struct PageTable(*mut u64);

impl PageTable {
    fn alloc() -> Self {
        Self(PT_SLAB.lock().alloc_4k())
    }

    fn from_phys(phys: u64) -> Self {
        Self((phys + PHYS_OFFSET) as *mut u64)
    }

    fn phys(&self) -> u64 {
        self.0 as u64 - PHYS_OFFSET
    }

    fn as_ptr(&self) -> *mut u64 {
        self.0
    }

    fn read(&self, index: usize) -> u64 {
        unsafe { self.0.add(index).read() }
    }

    fn read_volatile(&self, index: usize) -> u64 {
        unsafe { self.0.add(index).read_volatile() }
    }

    fn write(&self, index: usize, value: u64) {
        unsafe { self.0.add(index).write(value); }
    }

    fn write_leaf_2m(&self, index: usize, phys: u64, flags: u64) {
        unsafe { self.0.add(index).write(phys | flags | PAGE_SIZE_BIT); }
    }

    /// Get or create a child page table at the given index.
    fn get_or_create(&self, index: usize, flags: u64) -> PageTable {
        let entry = self.read(index);
        if entry & PAGE_PRESENT != 0 {
            // Upgrade flags if needed (e.g. adding PAGE_USER to a kernel-only entry)
            let updated = entry | (flags & (PAGE_PRESENT | PAGE_WRITE | PAGE_USER));
            if updated != entry {
                self.write(index, updated);
            }
            PageTable::from_phys(entry & ADDR_MASK)
        } else {
            let child = PageTable::alloc();
            self.write(index, child.phys() | flags);
            child
        }
    }

    fn child(&self, index: usize) -> Option<PageTable> {
        let entry = self.read(index);
        if entry & PAGE_PRESENT != 0 {
            Some(PageTable::from_phys(entry & ADDR_MASK))
        } else {
            None
        }
    }
}

/// Extract PML4, PDPT, and PD indices from a virtual address.
#[inline]
fn indices(addr: u64) -> (usize, usize, usize) {
    (
        ((addr >> 39) & 0x1FF) as usize,
        ((addr >> 30) & 0x1FF) as usize,
        ((addr >> 21) & 0x1FF) as usize,
    )
}

// ---------------------------------------------------------------------------
// Kernel direct map
// ---------------------------------------------------------------------------

static KERNEL_PML4: AtomicPtr<u64> = AtomicPtr::new(core::ptr::null_mut());

/// Build kernel page tables: map all physical memory in the high half
/// using 2MB large pages. PML4[0..255] is empty.
pub(super) fn init(memory_map: &[MemoryMapEntry]) {
    let mut max_addr: u64 = MIN_PHYS_MAP;
    for entry in memory_map {
        if entry.end > max_addr {
            max_addr = entry.end;
        }
    }
    max_addr = (max_addr + PAGE_2M - 1) & !(PAGE_2M - 1);

    let pml4 = PageTable::alloc();

    let mut addr: u64 = 0;
    while addr < max_addr {
        let (pml4_idx, pdpt_idx, pd_idx) = indices(PHYS_OFFSET + addr);
        let pdpt = pml4.get_or_create(pml4_idx, PAGE_PRESENT | PAGE_WRITE);
        let pd = pdpt.get_or_create(pdpt_idx, PAGE_PRESENT | PAGE_WRITE);
        pd.write_leaf_2m(pd_idx, addr, PAGE_PRESENT | PAGE_WRITE);
        addr += PAGE_2M;
    }

    KERNEL_PML4.store(pml4.as_ptr(), Ordering::Release);
    unsafe { crate::arch::cpu::write_cr3_raw(pml4.phys()); }
}

/// Physical address of the kernel PML4. Used for idle/exit CR3.
pub(super) fn kernel_cr3() -> u64 {
    let ptr = KERNEL_PML4.load(Ordering::Acquire);
    ptr as u64 - PHYS_OFFSET
}

/// Map an MMIO region into the kernel direct map using 2MB pages.
pub(super) fn map_mmio(phys: u64, size: u64) {
    let pml4 = PageTable::from_phys(kernel_cr3());

    let start = phys & !(PAGE_2M - 1);
    let end = (phys + size + PAGE_2M - 1) & !(PAGE_2M - 1);
    let mut cur = start;
    while cur < end {
        let (pml4_idx, pdpt_idx, pd_idx) = indices(PHYS_OFFSET + cur);
        let pdpt = pml4.get_or_create(pml4_idx, PAGE_PRESENT | PAGE_WRITE);
        let pd = pdpt.get_or_create(pdpt_idx, PAGE_PRESENT | PAGE_WRITE);
        if pd.read(pd_idx) & PAGE_PRESENT == 0 {
            pd.write_leaf_2m(pd_idx, cur, PAGE_PRESENT | PAGE_WRITE);
        }
        cur += PAGE_2M;
    }
    crate::arch::cpu::flush_tlb();
    crate::arch::apic::tlb_shootdown();
}

// ---------------------------------------------------------------------------
// AddressSpace — per-process page tables
// ---------------------------------------------------------------------------

/// Per-process address space backed by a PML4 page table.
/// PML4[0..255] = user mappings, PML4[256..511] = shared kernel direct map.
pub struct AddressSpace {
    root: *mut u64,
}

unsafe impl Send for AddressSpace {}
unsafe impl Sync for AddressSpace {}

impl AddressSpace {
    /// Create a new address space with kernel entries shallow-copied.
    pub fn new() -> Self {
        let kernel = PageTable::from_phys(kernel_cr3());
        let pml4 = PageTable::alloc();

        for i in 256..512 {
            let entry = kernel.read(i);
            if entry & PAGE_PRESENT != 0 {
                pml4.write(i, entry);
            }
        }

        Self { root: pml4.as_ptr() }
    }

    /// Physical address of PML4 for CR3 writes.
    pub fn cr3(&self) -> u64 {
        self.root as u64 - PHYS_OFFSET
    }

    /// Map a 2MB page into user space.
    /// Asserts: vaddr is 2MB-aligned. PDE slot is empty (no double-map).
    pub fn map(&self, vaddr: UserAddr, page: &PhysPage, writable: bool) {
        let va = vaddr.raw();
        assert!(va & (PAGE_2M - 1) == 0, "map: vaddr {va:#x} not 2MB-aligned");

        let phys = page.phys();
        let pml4 = PageTable::from_phys(self.cr3());
        let (pml4_idx, pdpt_idx, pd_idx) = indices(va);

        let mut flags = PAGE_PRESENT | PAGE_USER;
        if writable { flags |= PAGE_WRITE; }

        let pdpt = pml4.get_or_create(pml4_idx, PAGE_PRESENT | PAGE_WRITE | PAGE_USER);
        let pd = pdpt.get_or_create(pdpt_idx, PAGE_PRESENT | PAGE_WRITE | PAGE_USER);

        let existing = pd.read(pd_idx);
        assert!(existing & PAGE_PRESENT == 0,
            "map: PDE already present at vaddr {va:#x} (existing={existing:#x})");

        pd.write_leaf_2m(pd_idx, phys, flags);
    }

    /// Unmap one 2MB page.
    pub fn unmap(&self, vaddr: UserAddr) {
        let va = vaddr.raw();
        assert!(va & (PAGE_2M - 1) == 0, "unmap: vaddr {va:#x} not 2MB-aligned");

        let pml4 = PageTable::from_phys(self.cr3());
        let (pml4_idx, pdpt_idx, pd_idx) = indices(va);

        if let Some(pdpt) = pml4.child(pml4_idx) {
            if let Some(pd) = pdpt.child(pdpt_idx) {
                pd.write(pd_idx, 0);
            }
        }
    }

    /// Check if a 2MB region is mapped.
    pub fn is_mapped(&self, vaddr: UserAddr) -> bool {
        let va = vaddr.raw() & !(PAGE_2M - 1);
        let pml4 = PageTable::from_phys(self.cr3());
        let (pml4_idx, pdpt_idx, pd_idx) = indices(va);
        let Some(pdpt) = pml4.child(pml4_idx) else { return false };
        let Some(pd) = pdpt.child(pdpt_idx) else { return false };
        pd.read(pd_idx) & PAGE_PRESENT != 0
    }

    /// Translate a user virtual address to a kernel-accessible pointer.
    /// Returns None if the page is not mapped.
    pub fn translate(&self, vaddr: UserAddr) -> Option<*mut u8> {
        let va = vaddr.raw();
        let pml4 = PageTable::from_phys(self.cr3());
        let (pml4_idx, pdpt_idx, pd_idx) = indices(va);
        let pdpt = pml4.child(pml4_idx)?;
        let pd = pdpt.child(pdpt_idx)?;
        let pde = pd.read(pd_idx);
        if pde & PAGE_PRESENT == 0 { return None; }
        let page_phys = pde & ADDR_MASK_2M;
        let offset = va & (PAGE_2M - 1);
        Some((page_phys + offset + PHYS_OFFSET) as *mut u8)
    }
}

impl Drop for AddressSpace {
    fn drop(&mut self) {
        let pml4 = PageTable::from_phys(self.cr3());
        // Free user-half page table pages (PML4[0..255]).
        // PML4[256..511] are shared kernel entries — don't touch.
        for pml4_idx in 0..256 {
            let Some(pdpt) = pml4.child(pml4_idx) else { continue };
            for pdpt_idx in 0..512 {
                let Some(pd) = pdpt.child(pdpt_idx) else { continue };
                // PD pages are 4KB from the slab — we don't free them individually.
                // They'll be reclaimed when the slab's backing 2MB page is freed.
                let _ = pd;
            }
            let _ = pdpt;
        }
        // The PML4 page itself is also from the slab — not individually freed.
    }
}

// ---------------------------------------------------------------------------
// Debug
// ---------------------------------------------------------------------------

fn has(entry: u64, flag: u64) -> u8 {
    if entry & flag != 0 { 1 } else { 0 }
}

/// Dump page table entries for an address. Lock-free for crash safety.
pub fn debug_page_walk(addr: u64) {
    let cr3 = crate::arch::cpu::read_cr3_raw();
    let pml4 = PageTable::from_phys(cr3);
    let pml4_idx = ((addr >> 39) & 0x1FF) as usize;
    let pdpt_idx = ((addr >> 30) & 0x1FF) as usize;
    let pd_idx = ((addr >> 21) & 0x1FF) as usize;
    let pt_idx = ((addr >> 12) & 0x1FF) as usize;

    log!("  Page walk for {:#x} [PML4={:#x} PML4[{}] PDPT[{}] PD[{}] PT[{}]]:",
        addr, cr3, pml4_idx, pdpt_idx, pd_idx, pt_idx);

    let pml4e = pml4.read_volatile(pml4_idx);
    log!("    PML4E: {:#018x} P={} W={} U={}", pml4e,
        has(pml4e, PAGE_PRESENT), has(pml4e, PAGE_WRITE), has(pml4e, PAGE_USER));
    if pml4e & PAGE_PRESENT == 0 { return; }

    let pdpt = PageTable::from_phys(pml4e & ADDR_MASK);
    let pdpte = pdpt.read_volatile(pdpt_idx);
    log!("    PDPTE: {:#018x} P={} W={} U={}", pdpte,
        has(pdpte, PAGE_PRESENT), has(pdpte, PAGE_WRITE), has(pdpte, PAGE_USER));
    if pdpte & PAGE_PRESENT == 0 { return; }

    let pd = PageTable::from_phys(pdpte & ADDR_MASK);
    let pde = pd.read_volatile(pd_idx);
    log!("    PDE:   {:#018x} P={} W={} U={} PS={}", pde,
        has(pde, PAGE_PRESENT), has(pde, PAGE_WRITE), has(pde, PAGE_USER), has(pde, PAGE_SIZE_BIT));
    if pde & PAGE_PRESENT == 0 { return; }
    if pde & PAGE_SIZE_BIT != 0 {
        log!("    -> 2MB large page at {:#x}", pde & ADDR_MASK_2M);
        return;
    }

    let pt = PageTable::from_phys(pde & ADDR_MASK);
    let pte = pt.read_volatile(pt_idx);
    log!("    PTE:   {:#018x} P={} W={} U={}", pte,
        has(pte, PAGE_PRESENT), has(pte, PAGE_WRITE), has(pte, PAGE_USER));
    if pte & PAGE_PRESENT == 0 { return; }
    log!("    -> 4KB page at {:#x}", pte & ADDR_MASK);
}
