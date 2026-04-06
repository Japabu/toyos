// Page tables and address spaces.
//
// The only code that writes page table entries. Manages the kernel direct map
// (all physical memory at PHYS_OFFSET) and per-process user address spaces.

use core::sync::atomic::{AtomicU16, Ordering};

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use super::{UserAddr, PAGE_2M};
use crate::sync::Lock;
use crate::vma::{self, Region, RegionKind};
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

    /// Set a leaf PDE (2MB page) and invalidate the TLB entry.
    /// This is the ONLY way to write a user-space PDE.
    fn set_pde(&mut self, idx: usize, value: u64, va: u64) {
        self.0[idx] = value;
        invlpg(va);
    }

    /// Clear a leaf PDE and invalidate the TLB entry.
    fn clear_pde(&mut self, idx: usize, va: u64) {
        self.0[idx] = 0;
        invlpg(va);
    }

    /// Set a non-leaf entry (PML4E, PDPTE) or kernel entry. No TLB invalidation —
    /// caller is responsible for flushing when needed (e.g. flush_tlb_all after batch).
    fn set_entry(&mut self, idx: usize, value: u64) {
        self.0[idx] = value;
    }

    /// OR flags into an existing non-leaf entry (e.g. adding PAGE_WRITE to a PML4E).
    fn or_flags(&mut self, idx: usize, flags: u64) {
        self.0[idx] |= flags;
    }
}

impl core::ops::Index<usize> for PageTablePage {
    type Output = u64;
    fn index(&self, idx: usize) -> &u64 {
        &self.0[idx]
    }
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
// PCID and TLB management
// ---------------------------------------------------------------------------

use core::sync::atomic::AtomicBool;

const CR3_NOFLUSH: u64 = 1 << 63;
const CR3_ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

/// True when CR4.PCIDE + INVPCID are active.
static PCID_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Enable PCID if the CPU supports both PCID and INVPCID.
/// Without INVPCID there's no way to flush all PCIDs, so PCID alone is useless.
/// Must be called on each CPU. CR3 must have PCID 0 when called.
pub fn enable_pcid() {
    use crate::arch::cpu;
    if cpu::enable_pcid() {
        PCID_ACTIVE.store(true, Ordering::Relaxed);
        crate::log!("cpu: PCID + INVPCID enabled");
    } else {
        crate::log!("cpu: PCID not available, context switches will flush TLB");
    }
}

pub fn pcid_active() -> bool {
    PCID_ACTIVE.load(Ordering::Relaxed)
}

/// Flush all TLB entries on this CPU, all PCIDs.
pub fn flush_tlb_all() {
    if pcid_active() {
        crate::arch::cpu::invpcid(2, 0, 0);
    } else {
        unsafe {
            let cr3 = crate::arch::cpu::read_cr3();
            crate::arch::cpu::write_cr3(cr3);
        }
    }
}

/// Invalidate a single TLB entry for the given address.
pub fn invlpg(addr: u64) {
    if pcid_active() {
        let pcid = crate::arch::cpu::read_cr3() & 0xFFF;
        crate::arch::cpu::invpcid(0, pcid, addr);
    } else {
        crate::arch::cpu::invlpg(addr);
    }
}

/// CR3 register value: PML4 physical address | PCID.
#[derive(Clone, Copy)]
pub struct Cr3(u64);

impl Cr3 {
    pub fn current() -> Self {
        Self(crate::arch::cpu::read_cr3())
    }

    pub fn phys(self) -> u64 {
        self.0 & CR3_ADDR_MASK
    }
    pub fn pcid(self) -> u16 {
        (self.0 & 0xFFF) as u16
    }

    /// Switch to this address space. With PCID, sets NOFLUSH to preserve
    /// other processes' TLB entries. Without PCID, plain CR3 write.
    ///
    /// # Safety
    /// The underlying page tables must be valid and live.
    pub unsafe fn activate(self) {
        if pcid_active() {
            crate::arch::cpu::write_cr3(self.0 | CR3_NOFLUSH);
        } else {
            crate::arch::cpu::write_cr3(self.0);
        }
    }

    /// Load CR3 with a TLB flush. Used during boot before PCID is enabled.
    ///
    /// # Safety
    /// The underlying page tables must be valid and live.
    pub unsafe fn load_flush(self) {
        crate::arch::cpu::write_cr3(self.0);
    }
}

// ---------------------------------------------------------------------------
// PCID allocator
// ---------------------------------------------------------------------------

/// Next PCID to allocate. Range 1..4095. PCID 0 is reserved for the kernel.
static NEXT_PCID: AtomicU16 = AtomicU16::new(1);

/// Allocate a unique PCID for a new user address space.
/// On wrap past 4095, flushes all TLBs on all CPUs before recycling.
fn alloc_pcid() -> u16 {
    loop {
        let cur = NEXT_PCID.load(Ordering::Relaxed);
        if cur >= 1 && cur <= 4095 {
            match NEXT_PCID.compare_exchange(cur, cur + 1, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => return cur,
                Err(_) => continue,
            }
        }
        // Wrapped past 4095 — flush before recycling to prevent stale TLB hits.
        match NEXT_PCID.compare_exchange(cur, 2, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => {
                flush_tlb_all();
                crate::arch::apic::tlb_shootdown();
                return 1;
            }
            Err(_) => continue,
        }
    }
}

// ---------------------------------------------------------------------------
// AddressSpace
// ---------------------------------------------------------------------------

/// Unified address space: hardware page tables + virtual memory region tracking.
///
/// PML4[0..255] = user mappings (per-process), PML4[256..511] = kernel direct map (shared).
/// `regions` tracks all mapped virtual memory areas (ELF segments, mmap, stack, etc.)
/// and serves as the source of truth for the virtual address allocator.
pub struct AddressSpace {
    root: Box<PageTablePage>,
    children: Vec<Box<PageTablePage>>,
    /// Physical data pages mapped into user space. Freed on drop.
    pages: Vec<super::pmm::PhysPage>,
    /// All virtual memory regions, keyed by start address.
    regions: BTreeMap<UserAddr, Region>,
    /// PCID for this address space. 0 = kernel, 1..4095 = user.
    pcid: u16,
}

unsafe impl Send for AddressSpace {}
unsafe impl Sync for AddressSpace {}

fn align_up_2m(v: u64) -> u64 {
    (v + PAGE_2M - 1) & !(PAGE_2M - 1)
}

impl AddressSpace {
    /// Create a new user address space with kernel entries shallow-copied.
    pub fn new_user() -> Self {
        let guard = kernel().lock();
        let kernel_as = guard.as_ref().expect("paging not initialized");
        let mut pml4 = Box::new(PageTablePage([0; 512]));

        for i in 256..512 {
            if kernel_as.root[i] & PAGE_PRESENT != 0 {
                pml4.set_entry(i, kernel_as.root[i]);
            }
        }

        Self {
            root: pml4,
            children: Vec::new(),
            pages: Vec::new(),
            regions: BTreeMap::new(),
            pcid: alloc_pcid(),
        }
    }

    pub fn cr3(&self) -> Cr3 {
        Cr3(self.root.phys() | self.pcid as u64)
    }

    /// Map a contiguous physical region into user space as 2MB pages.
    /// Asserts: vaddr and phys are 2MB-aligned, all PDE slots are empty.
    pub fn map_range(&mut self, vaddr: UserAddr, phys: u64, size: u64, writable: bool) {
        assert!(
            vaddr.raw() & (PAGE_2M - 1) == 0,
            "map_range: vaddr not 2MB-aligned"
        );
        assert!(
            phys & (PAGE_2M - 1) == 0,
            "map_range: phys {phys:#x} not 2MB-aligned"
        );
        let mut offset = 0u64;
        while offset < size {
            let va = vaddr.raw() + offset;
            let pa = phys + offset;
            let mut flags = PAGE_PRESENT | PAGE_USER;
            if writable {
                flags |= PAGE_WRITE;
            }
            let (pml4_idx, pdpt_idx, pd_idx) = indices(va);
            let user_flags = PAGE_PRESENT | PAGE_WRITE | PAGE_USER;
            let pd = self.ensure_table(pml4_idx, user_flags, pdpt_idx, user_flags);
            let existing = pd[pd_idx];
            assert!(
                existing & PAGE_PRESENT == 0,
                "map_range: PDE already present at vaddr {va:#x} (existing={existing:#x})"
            );
            pd.set_pde(pd_idx, pa | flags | PAGE_SIZE_BIT, va);
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
        assert!(
            va & (PAGE_2M - 1) == 0,
            "remap: vaddr {va:#x} not 2MB-aligned"
        );
        assert!(
            phys & (PAGE_2M - 1) == 0,
            "remap: phys {phys:#x} not 2MB-aligned"
        );

        let mut flags = PAGE_PRESENT | PAGE_USER;
        if writable {
            flags |= PAGE_WRITE;
        }

        let (pml4_idx, pdpt_idx, pd_idx) = indices(va);
        let user_flags = PAGE_PRESENT | PAGE_WRITE | PAGE_USER;
        let pd = self.ensure_table(pml4_idx, user_flags, pdpt_idx, user_flags);
        pd.set_pde(pd_idx, phys | flags | PAGE_SIZE_BIT, va);
    }

    /// Allocate a 2MB page from PMM and map it at `vaddr`.
    /// Returns a DirectMap handle for kernel access to the page.
    pub fn map_alloc(&mut self, vaddr: UserAddr, writable: bool) -> super::DirectMap {
        let page = super::pmm::alloc_page(super::pmm::Category::PageTable)
            .expect("map_alloc: out of physical memory");
        let phys = page.phys();
        let dm = page.direct_map();

        let va = vaddr.raw();
        assert!(
            va & (PAGE_2M - 1) == 0,
            "map_alloc: vaddr {va:#x} not 2MB-aligned"
        );

        let mut flags = PAGE_PRESENT | PAGE_USER;
        if writable {
            flags |= PAGE_WRITE;
        }

        let (pml4_idx, pdpt_idx, pd_idx) = indices(va);
        let user_flags = PAGE_PRESENT | PAGE_WRITE | PAGE_USER;
        let pd = self.ensure_table(pml4_idx, user_flags, pdpt_idx, user_flags);
        assert!(
            pd[pd_idx] & PAGE_PRESENT == 0,
            "map_alloc: PDE already present at vaddr {va:#x}"
        );
        pd.set_pde(pd_idx, phys | flags | PAGE_SIZE_BIT, va);
        self.pages.push(page);
        dm
    }

    /// Unmap one 2MB page and free its physical memory.
    pub fn unmap(&mut self, vaddr: UserAddr) {
        let va = vaddr.raw();
        assert!(
            va & (PAGE_2M - 1) == 0,
            "unmap: vaddr {va:#x} not 2MB-aligned"
        );

        let (pml4_idx, pdpt_idx, pd_idx) = indices(va);

        if let Some(pdpt) = self.root.child_mut(pml4_idx) {
            if let Some(pd) = pdpt.child_mut(pdpt_idx) {
                let pde = pd[pd_idx];
                if pde & PAGE_PRESENT != 0 {
                    pd.clear_pde(pd_idx, va);
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
        let Some(pdpt) = self.root.child(pml4_idx) else {
            return false;
        };
        let Some(pd) = pdpt.child(pdpt_idx) else {
            return false;
        };
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
        if pde & PAGE_PRESENT == 0 {
            return None;
        }
        let page_phys = pde & ADDR_MASK_2M;
        let offset = va & (PAGE_2M - 1);
        Some(super::DirectMap::from_phys(page_phys + offset))
    }

    // -----------------------------------------------------------------------
    // Virtual memory region management
    // -----------------------------------------------------------------------

    /// Find a free gap of at least `size` bytes (2MB-aligned), searching top-down.
    fn find_gap(&self, size: u64) -> Option<UserAddr> {
        let aligned = align_up_2m(size);
        let total = aligned + vma::GUARD_SIZE;

        let mut top = vma::ALLOC_CEILING;
        for (&start, region) in self
            .regions
            .range(..UserAddr::new(vma::ALLOC_CEILING))
            .rev()
        {
            let region_end = align_up_2m(start.raw() + region.size);
            if region_end > top {
                top = start.raw();
                continue;
            }
            let gap = top - region_end;
            if gap >= total {
                return Some(UserAddr::new(top - total));
            }
            top = start.raw();
        }
        // Gap below all regions
        if top >= total + vma::ALLOC_FLOOR {
            return Some(UserAddr::new(top - total));
        }
        None
    }

    /// Allocate a virtual address range and register the region.
    pub fn alloc_region(
        &mut self,
        size: u64,
        kind: RegionKind,
        writable: bool,
    ) -> Option<UserAddr> {
        let aligned = align_up_2m(size);
        let addr = self.find_gap(aligned)?;
        self.regions.insert(
            addr,
            Region {
                size: aligned,
                writable,
                kind,
            },
        );
        Some(addr)
    }

    /// Allocate a region and map physical memory into it.
    pub fn alloc_and_map(&mut self, phys: u64, size: u64) -> Option<(UserAddr, u64)> {
        let aligned = align_up_2m(size);
        assert!(
            phys & (PAGE_2M - 1) == 0,
            "alloc_and_map: phys {phys:#x} not 2MB-aligned"
        );
        let addr = self.find_gap(aligned)?;
        self.regions.insert(
            addr,
            Region {
                size: aligned,
                writable: true,
                kind: RegionKind::Mapped,
            },
        );
        self.map_range(addr, phys, aligned, true);
        Some((addr, aligned))
    }

    /// Free a previously allocated region and unmap it.
    pub fn free_and_unmap(&mut self, addr: UserAddr) -> Option<u64> {
        let size = self.regions.remove(&addr)?.size;
        self.unmap_range(addr, size);
        Some(size)
    }

    /// Free a region without unmapping (for demand-paged regions where pages
    /// are tracked separately).
    pub fn free_region(&mut self, addr: UserAddr) -> Option<u64> {
        self.regions.remove(&addr).map(|r| r.size)
    }

    /// Insert a region at a specific address (for ELF segments, stack, etc.)
    pub fn insert_region(&mut self, addr: UserAddr, region: Region) {
        assert!(
            self.find_region(addr).is_none(),
            "insert_region: address {:#x} already occupied",
            addr.raw()
        );
        self.regions.insert(addr, region);
    }

    /// Find the region containing `addr`. Returns (start_addr, region).
    pub fn find_region(&self, addr: UserAddr) -> Option<(UserAddr, &Region)> {
        let (&start, region) = self.regions.range(..=addr).next_back()?;
        if addr.raw() < start.raw() + region.size {
            Some((start, region))
        } else {
            None
        }
    }

    /// Iterate all regions that overlap the range [start, end).
    pub fn overlapping_regions(
        &self,
        start: UserAddr,
        end: UserAddr,
    ) -> impl Iterator<Item = (&UserAddr, &Region)> {
        // A region starting at s with size n overlaps [start, end) iff s < end && s+n > start.
        // Use range(..end) to skip regions starting at or after end, then filter the lower bound.
        self.regions
            .range(..end)
            .filter(move |(&s, r)| s.raw() + r.size > start.raw())
    }

    /// Clear all regions (for process teardown).
    pub fn clear_regions(&mut self) {
        self.regions.clear();
    }

    // -----------------------------------------------------------------------
    // Direct map / MMIO
    // -----------------------------------------------------------------------

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
        flush_tlb_all();
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
        flush_tlb_all();
        crate::arch::apic::tlb_shootdown();
    }

    fn map_2m(&mut self, phys: u64, flags: u64) {
        let virt = super::DirectMap::from_phys(phys).as_ptr::<u8>() as u64;
        let (pml4_idx, pdpt_idx, pd_idx) = indices(virt);
        let pd = self.ensure_table(pml4_idx, flags, pdpt_idx, flags);
        if pd[pd_idx] & PAGE_PRESENT == 0 {
            pd.set_entry(pd_idx, phys | flags | PAGE_SIZE_BIT);
        }
    }

    fn unmap_2m(&mut self, phys: u64) {
        let virt = super::DirectMap::from_phys(phys).as_ptr::<u8>() as u64;
        let (pml4_idx, pdpt_idx, pd_idx) = indices(virt);
        if let Some(pdpt) = self.root.child_mut(pml4_idx) {
            if let Some(pd) = pdpt.child_mut(pdpt_idx) {
                pd.set_entry(pd_idx, 0);
            }
        }
    }

    fn ensure_table(
        &mut self,
        pml4_idx: usize,
        pml4_flags: u64,
        pdpt_idx: usize,
        pdpt_flags: u64,
    ) -> &mut PageTablePage {
        if self.root[pml4_idx] & PAGE_PRESENT == 0 {
            let child = Box::new(PageTablePage([0; 512]));
            self.root.set_entry(pml4_idx, child.phys() | pml4_flags);
            self.children.push(child);
        } else {
            self.root.or_flags(pml4_idx, pml4_flags & (PAGE_PRESENT | PAGE_WRITE | PAGE_USER));
        }

        let pdpt = unsafe { PageTablePage::from_phys_mut(self.root[pml4_idx] & ADDR_MASK) };

        if pdpt[pdpt_idx] & PAGE_PRESENT == 0 {
            let child = Box::new(PageTablePage([0; 512]));
            pdpt.set_entry(pdpt_idx, child.phys() | pdpt_flags);
            self.children.push(child);
        } else {
            pdpt.or_flags(pdpt_idx, pdpt_flags & (PAGE_PRESENT | PAGE_WRITE | PAGE_USER));
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
pub fn kernel_cr3() -> Cr3 {
    Cr3(KERNEL_CR3.load(core::sync::atomic::Ordering::Relaxed))
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

    let mut kernel = AddressSpace {
        root: Box::new(PageTablePage([0; 512])),
        children: Vec::new(),
        pages: Vec::new(),
        regions: BTreeMap::new(),
        pcid: 0, // Kernel always uses PCID 0
    };

    let mut addr: u64 = 0;
    while addr < max_addr {
        kernel.map_2m(addr, PAGE_PRESENT | PAGE_WRITE);
        addr += PAGE_2M;
    }

    let cr3 = kernel.cr3();
    KERNEL_CR3.store(cr3.0, core::sync::atomic::Ordering::Release);
    *KERNEL.lock() = Some(kernel);
    // Boot path: load CR3 with flush (PCID not yet enabled).
    unsafe {
        cr3.load_flush();
    }
}

// ---------------------------------------------------------------------------
// Debug
// ---------------------------------------------------------------------------

fn has(entry: u64, flag: u64) -> u8 {
    if entry & flag != 0 {
        1
    } else {
        0
    }
}

/// Dump page table entries for an address. Lock-free for crash safety.
pub fn debug_page_walk(addr: u64) {
    let cr3 = Cr3::current();
    let pml4 = unsafe { PageTablePage::from_phys(cr3.phys()) };
    let pml4_idx = ((addr >> 39) & 0x1FF) as usize;
    let pdpt_idx = ((addr >> 30) & 0x1FF) as usize;
    let pd_idx = ((addr >> 21) & 0x1FF) as usize;
    let pt_idx = ((addr >> 12) & 0x1FF) as usize;

    log!(
        "  Page walk for {:#x} [PML4={:#x} PCID={} PML4[{}] PDPT[{}] PD[{}] PT[{}]]:",
        addr,
        cr3.phys(),
        cr3.pcid(),
        pml4_idx,
        pdpt_idx,
        pd_idx,
        pt_idx
    );

    let pml4e = pml4[pml4_idx];
    log!(
        "    PML4E: {:#018x} P={} W={} U={}",
        pml4e,
        has(pml4e, PAGE_PRESENT),
        has(pml4e, PAGE_WRITE),
        has(pml4e, PAGE_USER)
    );
    if pml4e & PAGE_PRESENT == 0 {
        return;
    }

    let pdpt = unsafe { PageTablePage::from_phys(pml4e & ADDR_MASK) };
    let pdpte = pdpt[pdpt_idx];
    log!(
        "    PDPTE: {:#018x} P={} W={} U={}",
        pdpte,
        has(pdpte, PAGE_PRESENT),
        has(pdpte, PAGE_WRITE),
        has(pdpte, PAGE_USER)
    );
    if pdpte & PAGE_PRESENT == 0 {
        return;
    }

    let pd = unsafe { PageTablePage::from_phys(pdpte & ADDR_MASK) };
    let pde = pd[pd_idx];
    log!(
        "    PDE:   {:#018x} P={} W={} U={} PS={}",
        pde,
        has(pde, PAGE_PRESENT),
        has(pde, PAGE_WRITE),
        has(pde, PAGE_USER),
        has(pde, PAGE_SIZE_BIT)
    );
    if pde & PAGE_PRESENT == 0 {
        return;
    }
    if pde & PAGE_SIZE_BIT != 0 {
        log!("    -> 2MB large page at {:#x}", pde & ADDR_MASK_2M);
        return;
    }

    let pt = unsafe { PageTablePage::from_phys(pde & ADDR_MASK) };
    let pte = pt[pt_idx];
    log!(
        "    PTE:   {:#018x} P={} W={} U={}",
        pte,
        has(pte, PAGE_PRESENT),
        has(pte, PAGE_WRITE),
        has(pte, PAGE_USER)
    );
    if pte & PAGE_PRESENT == 0 {
        return;
    }
    log!("    -> 4KB page at {:#x}", pte & ADDR_MASK);
}
