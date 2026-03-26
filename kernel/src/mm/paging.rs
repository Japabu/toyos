// Page tables and address spaces.
//
// The only code that writes page table entries. Manages the kernel direct map
// (all physical memory at PHYS_OFFSET) and per-process user address spaces.

use alloc::boxed::Box;
use alloc::vec::Vec;

use super::{PAGE_2M, UserAddr};
use crate::sync::Lock;
use crate::MemoryMapEntry;

const PAGE_PRESENT: u64 = 1 << 0;
const PAGE_WRITE: u64 = 1 << 1;
const PAGE_USER: u64 = 1 << 2;
const PAGE_SIZE_BIT: u64 = 1 << 7;
const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;
const ADDR_MASK_2M: u64 = 0x000F_FFFF_FFE0_0000;

/// A 4KB-aligned page of 512 entries, matching the hardware page table format.
#[repr(C, align(4096))]
struct PageTablePage([u64; 512]);

impl PageTablePage {
    fn phys(&self) -> u64 {
        super::DirectMap::phys_of(self)
    }

    unsafe fn from_phys<'a>(phys: u64) -> &'a PageTablePage {
        &*super::DirectMap::from_phys(phys).as_ptr::<PageTablePage>()
    }

    unsafe fn from_phys_mut<'a>(phys: u64) -> &'a mut PageTablePage {
        &mut *super::DirectMap::from_phys(phys).as_mut_ptr::<PageTablePage>()
    }

    fn child(&self, index: usize) -> Option<&PageTablePage> {
        let entry = self[index];
        if entry & PAGE_PRESENT != 0 {
            Some(unsafe { PageTablePage::from_phys(entry & ADDR_MASK) })
        } else {
            None
        }
    }

    fn child_mut(&mut self, index: usize) -> Option<&mut PageTablePage> {
        let entry = self[index];
        if entry & PAGE_PRESENT != 0 {
            Some(unsafe { PageTablePage::from_phys_mut(entry & ADDR_MASK) })
        } else {
            None
        }
    }
}

impl core::ops::Index<usize> for PageTablePage {
    type Output = u64;
    fn index(&self, idx: usize) -> &u64 { &self.0[idx] }
}

impl core::ops::IndexMut<usize> for PageTablePage {
    fn index_mut(&mut self, idx: usize) -> &mut u64 { &mut self.0[idx] }
}

#[inline]
fn indices(addr: u64) -> (usize, usize, usize) {
    (
        ((addr >> 39) & 0x1FF) as usize,
        ((addr >> 30) & 0x1FF) as usize,
        ((addr >> 21) & 0x1FF) as usize,
    )
}

// ---------------------------------------------------------------------------
// AddressSpace
// ---------------------------------------------------------------------------

/// Address space backed by a PML4 page table.
/// PML4[0..255] = user mappings (per-process), PML4[256..511] = kernel direct map (shared).
/// All child page table pages are owned by `children` and freed automatically on drop.
pub struct AddressSpace {
    root: Box<PageTablePage>,
    children: Vec<Box<PageTablePage>>,
    /// Physical data pages mapped into user space. Freed on drop.
    pages: Vec<super::pmm::PhysPage>,
}

unsafe impl Send for AddressSpace {}
unsafe impl Sync for AddressSpace {}

impl AddressSpace {
    fn empty() -> Self {
        Self {
            root: Box::new(PageTablePage([0; 512])),
            children: Vec::new(),
            pages: Vec::new(),
        }
    }

    /// Create a new user address space with kernel entries shallow-copied.
    pub fn new_user() -> Self {
        let guard = kernel().lock();
        let kernel_as = guard.as_ref().expect("paging not initialized");
        let mut pml4 = Box::new(PageTablePage([0; 512]));

        for i in 256..512 {
            if kernel_as.root[i] & PAGE_PRESENT != 0 {
                pml4[i] = kernel_as.root[i];
            }
        }

        Self { root: pml4, children: Vec::new(), pages: Vec::new() }
    }

    /// Physical address of PML4 for CR3 writes.
    pub fn cr3(&self) -> u64 {
        self.root.phys()
    }

    /// Map a contiguous physical region into user space as 2MB pages.
    /// Asserts: vaddr and phys are 2MB-aligned, all PDE slots are empty.
    pub fn map_range(&mut self, vaddr: UserAddr, phys: u64, size: u64, writable: bool) {
        assert!(vaddr.raw() & (PAGE_2M - 1) == 0, "map_range: vaddr not 2MB-aligned");
        assert!(phys & (PAGE_2M - 1) == 0, "map_range: phys {phys:#x} not 2MB-aligned");
        let mut offset = 0u64;
        while offset < size {
            let va = vaddr.raw() + offset;
            let pa = phys + offset;
            let mut flags = PAGE_PRESENT | PAGE_USER;
            if writable { flags |= PAGE_WRITE; }
            let (pml4_idx, pdpt_idx, pd_idx) = indices(va);
            let user_flags = PAGE_PRESENT | PAGE_WRITE | PAGE_USER;
            let pd = self.ensure_table(pml4_idx, user_flags, pdpt_idx, user_flags);
            let existing = pd[pd_idx];
            assert!(existing & PAGE_PRESENT == 0,
                "map_range: PDE already present at vaddr {va:#x} (existing={existing:#x})");
            pd[pd_idx] = pa | flags | PAGE_SIZE_BIT;
            offset += PAGE_2M;
        }
    }

    /// Unmap a contiguous range of 2MB pages.
    pub fn unmap_range(&mut self, vaddr: UserAddr, size: u64) {
        let mut offset = 0u64;
        while offset < size {
            self.unmap(UserAddr::new(vaddr.raw() + offset));
            offset += PAGE_2M;
        }
    }

    /// Map a single 2MB page, replacing any existing mapping.
    /// Used by demand paging and shared library RW overlay.
    pub fn remap(&mut self, vaddr: UserAddr, phys: u64, writable: bool) {
        let va = vaddr.raw();
        assert!(va & (PAGE_2M - 1) == 0, "remap: vaddr {va:#x} not 2MB-aligned");
        assert!(phys & (PAGE_2M - 1) == 0, "remap: phys {phys:#x} not 2MB-aligned");

        let mut flags = PAGE_PRESENT | PAGE_USER;
        if writable { flags |= PAGE_WRITE; }

        let (pml4_idx, pdpt_idx, pd_idx) = indices(va);
        let user_flags = PAGE_PRESENT | PAGE_WRITE | PAGE_USER;
        let pd = self.ensure_table(pml4_idx, user_flags, pdpt_idx, user_flags);
        pd[pd_idx] = phys | flags | PAGE_SIZE_BIT;
    }

    /// Allocate a 2MB page from PMM and map it at `vaddr`.
    /// Returns a DirectMap handle for kernel access to the page.
    pub fn map_alloc(&mut self, vaddr: UserAddr, writable: bool) -> super::DirectMap {
        let page = super::pmm::alloc_page().expect("map_alloc: out of physical memory");
        let phys = page.phys();
        let dm = page.direct_map();

        let va = vaddr.raw();
        assert!(va & (PAGE_2M - 1) == 0, "map_alloc: vaddr {va:#x} not 2MB-aligned");

        let mut flags = PAGE_PRESENT | PAGE_USER;
        if writable { flags |= PAGE_WRITE; }

        let (pml4_idx, pdpt_idx, pd_idx) = indices(va);
        let user_flags = PAGE_PRESENT | PAGE_WRITE | PAGE_USER;
        let pd = self.ensure_table(pml4_idx, user_flags, pdpt_idx, user_flags);
        assert!(pd[pd_idx] & PAGE_PRESENT == 0,
            "map_alloc: PDE already present at vaddr {va:#x}");
        pd[pd_idx] = phys | flags | PAGE_SIZE_BIT;
        self.pages.push(page);
        dm
    }

    /// Unmap one 2MB page and free its physical memory.
    pub fn unmap(&mut self, vaddr: UserAddr) {
        let va = vaddr.raw();
        assert!(va & (PAGE_2M - 1) == 0, "unmap: vaddr {va:#x} not 2MB-aligned");

        let (pml4_idx, pdpt_idx, pd_idx) = indices(va);

        if let Some(pdpt) = self.root.child_mut(pml4_idx) {
            if let Some(pd) = pdpt.child_mut(pdpt_idx) {
                let pde = pd[pd_idx];
                if pde & PAGE_PRESENT != 0 {
                    pd[pd_idx] = 0;
                    let phys = pde & ADDR_MASK_2M;
                    // Remove the page from our owned list — Drop frees it
                    self.pages.retain(|p| p.phys() != phys);
                }
            }
        }
    }

    /// Check if a 2MB region is mapped.
    pub fn is_mapped(&self, vaddr: UserAddr) -> bool {
        let va = vaddr.raw() & !(PAGE_2M - 1);
        let (pml4_idx, pdpt_idx, pd_idx) = indices(va);
        let Some(pdpt) = self.root.child(pml4_idx) else { return false };
        let Some(pd) = pdpt.child(pdpt_idx) else { return false };
        pd[pd_idx] & PAGE_PRESENT != 0
    }

    /// Translate a user virtual address to a DirectMap handle.
    /// Returns None if the page is not mapped.
    pub fn translate(&self, vaddr: UserAddr) -> Option<super::DirectMap> {
        let va = vaddr.raw();
        let (pml4_idx, pdpt_idx, pd_idx) = indices(va);
        let pdpt = self.root.child(pml4_idx)?;
        let pd = pdpt.child(pdpt_idx)?;
        let pde = pd[pd_idx];
        if pde & PAGE_PRESENT == 0 { return None; }
        let page_phys = pde & ADDR_MASK_2M;
        let offset = va & (PAGE_2M - 1);
        Some(super::DirectMap::from_phys(page_phys + offset))
    }

    /// Map a physical region into the direct map using 2MB pages.
    /// Returns an Mmio handle for bounds-checked register access.
    pub fn map_mmio(&mut self, phys: u64, size: u64) -> super::Mmio {
        let start = phys & !(PAGE_2M - 1);
        let end = (phys + size + PAGE_2M - 1) & !(PAGE_2M - 1);
        let mut cur = start;
        while cur < end {
            self.map_2m(cur, PAGE_PRESENT | PAGE_WRITE);
            cur += PAGE_2M;
        }
        crate::arch::cpu::flush_tlb();
        crate::arch::apic::tlb_shootdown();
        super::Mmio::new(super::DirectMap::from_phys(phys), size)
    }

    /// Unmap a physical region from the direct map.
    pub fn unmap_mmio(&mut self, phys: u64, size: u64) {
        let start = phys & !(PAGE_2M - 1);
        let end = (phys + size + PAGE_2M - 1) & !(PAGE_2M - 1);
        let mut cur = start;
        while cur < end {
            self.unmap_2m(cur);
            cur += PAGE_2M;
        }
        crate::arch::cpu::flush_tlb();
        crate::arch::apic::tlb_shootdown();
    }

    fn map_2m(&mut self, phys: u64, flags: u64) {
        let virt = super::DirectMap::from_phys(phys).as_ptr::<u8>() as u64;
        let (pml4_idx, pdpt_idx, pd_idx) = indices(virt);
        let pd = self.ensure_table(pml4_idx, flags, pdpt_idx, flags);
        if pd[pd_idx] & PAGE_PRESENT == 0 {
            pd[pd_idx] = phys | flags | PAGE_SIZE_BIT;
        }
    }

    fn unmap_2m(&mut self, phys: u64) {
        let virt = super::DirectMap::from_phys(phys).as_ptr::<u8>() as u64;
        let (pml4_idx, pdpt_idx, pd_idx) = indices(virt);
        if let Some(pdpt) = self.root.child_mut(pml4_idx) {
            if let Some(pd) = pdpt.child_mut(pdpt_idx) {
                pd[pd_idx] = 0;
            }
        }
    }

    fn ensure_table(&mut self, pml4_idx: usize, pml4_flags: u64,
                    pdpt_idx: usize, pdpt_flags: u64) -> &mut PageTablePage {
        if self.root[pml4_idx] & PAGE_PRESENT == 0 {
            let child = Box::new(PageTablePage([0; 512]));
            self.root[pml4_idx] = child.phys() | pml4_flags;
            self.children.push(child);
        } else {
            self.root[pml4_idx] |= pml4_flags & (PAGE_PRESENT | PAGE_WRITE | PAGE_USER);
        }

        let pdpt = unsafe { PageTablePage::from_phys_mut(self.root[pml4_idx] & ADDR_MASK) };

        if pdpt[pdpt_idx] & PAGE_PRESENT == 0 {
            let child = Box::new(PageTablePage([0; 512]));
            pdpt[pdpt_idx] = child.phys() | pdpt_flags;
            self.children.push(child);
        } else {
            pdpt[pdpt_idx] |= pdpt_flags & (PAGE_PRESENT | PAGE_WRITE | PAGE_USER);
        }

        unsafe { PageTablePage::from_phys_mut(pdpt[pdpt_idx] & ADDR_MASK) }
    }
}

// ---------------------------------------------------------------------------
// Kernel address space
// ---------------------------------------------------------------------------

const MIN_PHYS_MAP: u64 = 4 * 1024 * 1024 * 1024;

static KERNEL: Lock<Option<AddressSpace>> = Lock::new(None);

/// Kernel CR3, cached for lock-free access from panic/crash paths.
static KERNEL_CR3: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// The kernel address space. Mapped once at boot, lives forever.
pub fn kernel() -> &'static Lock<Option<AddressSpace>> {
    &KERNEL
}

/// Kernel CR3. Lock-free — safe to call from panic context.
pub fn kernel_cr3() -> u64 {
    KERNEL_CR3.load(core::sync::atomic::Ordering::Relaxed)
}

/// Build kernel page tables: map all physical memory in the high half using 2MB large pages.
pub(super) fn init(memory_map: &[MemoryMapEntry]) {
    let mut max_addr: u64 = MIN_PHYS_MAP;
    for entry in memory_map {
        if entry.end > max_addr {
            max_addr = entry.end;
        }
    }
    max_addr = (max_addr + PAGE_2M - 1) & !(PAGE_2M - 1);

    let mut kernel = AddressSpace::empty();

    let mut addr: u64 = 0;
    while addr < max_addr {
        kernel.map_2m(addr, PAGE_PRESENT | PAGE_WRITE);
        addr += PAGE_2M;
    }

    let cr3 = kernel.cr3();
    KERNEL_CR3.store(cr3, core::sync::atomic::Ordering::Release);
    *KERNEL.lock() = Some(kernel);
    unsafe { crate::arch::cpu::write_cr3(cr3); }
}

// ---------------------------------------------------------------------------
// Debug
// ---------------------------------------------------------------------------

fn has(entry: u64, flag: u64) -> u8 {
    if entry & flag != 0 { 1 } else { 0 }
}

/// Dump page table entries for an address. Lock-free for crash safety.
pub fn debug_page_walk(addr: u64) {
    let cr3 = crate::arch::cpu::read_cr3();
    let pml4 = unsafe { PageTablePage::from_phys(cr3) };
    let pml4_idx = ((addr >> 39) & 0x1FF) as usize;
    let pdpt_idx = ((addr >> 30) & 0x1FF) as usize;
    let pd_idx = ((addr >> 21) & 0x1FF) as usize;
    let pt_idx = ((addr >> 12) & 0x1FF) as usize;

    log!("  Page walk for {:#x} [PML4={:#x} PML4[{}] PDPT[{}] PD[{}] PT[{}]]:",
        addr, cr3, pml4_idx, pdpt_idx, pd_idx, pt_idx);

    let pml4e = pml4[pml4_idx];
    log!("    PML4E: {:#018x} P={} W={} U={}", pml4e,
        has(pml4e, PAGE_PRESENT), has(pml4e, PAGE_WRITE), has(pml4e, PAGE_USER));
    if pml4e & PAGE_PRESENT == 0 { return; }

    let pdpt = unsafe { PageTablePage::from_phys(pml4e & ADDR_MASK) };
    let pdpte = pdpt[pdpt_idx];
    log!("    PDPTE: {:#018x} P={} W={} U={}", pdpte,
        has(pdpte, PAGE_PRESENT), has(pdpte, PAGE_WRITE), has(pdpte, PAGE_USER));
    if pdpte & PAGE_PRESENT == 0 { return; }

    let pd = unsafe { PageTablePage::from_phys(pdpte & ADDR_MASK) };
    let pde = pd[pd_idx];
    log!("    PDE:   {:#018x} P={} W={} U={} PS={}", pde,
        has(pde, PAGE_PRESENT), has(pde, PAGE_WRITE), has(pde, PAGE_USER), has(pde, PAGE_SIZE_BIT));
    if pde & PAGE_PRESENT == 0 { return; }
    if pde & PAGE_SIZE_BIT != 0 {
        log!("    -> 2MB large page at {:#x}", pde & ADDR_MASK_2M);
        return;
    }

    let pt = unsafe { PageTablePage::from_phys(pde & ADDR_MASK) };
    let pte = pt[pt_idx];
    log!("    PTE:   {:#018x} P={} W={} U={}", pte,
        has(pte, PAGE_PRESENT), has(pte, PAGE_WRITE), has(pte, PAGE_USER));
    if pte & PAGE_PRESENT == 0 { return; }
    log!("    -> 4KB page at {:#x}", pte & ADDR_MASK);
}
