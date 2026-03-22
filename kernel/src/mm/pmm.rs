use super::{DirectMap, PAGE_2M};
use crate::sync::Lock;
use crate::MemoryMapEntry;

/// Region of physical memory to exclude from the free list.
#[derive(Debug, Clone, Copy)]
pub struct Region {
    pub start: u64,
    pub end: u64,
}

/// Proof of ownership of one 2MB physical page. Non-Copy, non-Clone.
/// Drop returns the page to the free list.
pub struct PhysPage(u64); // raw physical address, 2MB-aligned

impl PhysPage {
    /// Reconstruct from a raw physical address. Caller must ensure this
    /// is a valid 2MB-aligned page that was previously allocated.
    pub(super) fn from_raw(phys: u64) -> Self {
        Self(phys)
    }

    /// Physical address (for page table entries, internal use only).
    pub(super) fn phys(&self) -> u64 {
        self.0
    }

    /// Access this page through the kernel direct map.
    pub fn direct_map(&self) -> super::DirectMap {
        super::DirectMap::from_phys(self.0)
    }

}

impl Drop for PhysPage {
    fn drop(&mut self) {
        free_page(self.0);
    }
}

// --- Bitmap allocator ---

/// Maximum physical memory: 64 GB → 32768 2MB pages → 4096 bytes bitmap.
const MAX_PAGES: usize = 32768;

struct Bitmap {
    /// One bit per 2MB page. 1 = free, 0 = allocated.
    bits: [u64; MAX_PAGES / 64],
    /// Physical address of page index 0 (lowest usable page).
    base: u64,
    /// Number of valid page indices (base..base + page_count * PAGE_2M).
    page_count: usize,
    free_count: usize,
    total_usable: usize,
}

impl Bitmap {
    const fn new() -> Self {
        Self {
            bits: [0; MAX_PAGES / 64],
            base: 0,
            page_count: 0,
            free_count: 0,
            total_usable: 0,
        }
    }

    fn set_free(&mut self, idx: usize) {
        self.bits[idx / 64] |= 1u64 << (idx % 64);
    }

    fn set_used(&mut self, idx: usize) {
        self.bits[idx / 64] &= !(1u64 << (idx % 64));
    }

    fn is_free(&self, idx: usize) -> bool {
        self.bits[idx / 64] & (1u64 << (idx % 64)) != 0
    }

    fn phys_to_idx(&self, phys: u64) -> usize {
        ((phys - self.base) / PAGE_2M) as usize
    }

    fn idx_to_phys(&self, idx: usize) -> u64 {
        self.base + idx as u64 * PAGE_2M
    }
}

static BITMAP: Lock<Bitmap> = Lock::new(Bitmap::new());

/// Initialize the bitmap from the UEFI memory map.
pub(super) fn init(entries: &[MemoryMapEntry], reserved: &[Region]) {
    let mut bm = BITMAP.lock();

    // Find the range of usable physical memory.
    let mut lo = u64::MAX;
    let mut hi = 0u64;
    for entry in entries.iter().filter(|e| is_usable(e)) {
        let start = (entry.start + PAGE_2M - 1) & !(PAGE_2M - 1);
        let end = entry.end & !(PAGE_2M - 1);
        if start < end {
            lo = lo.min(start);
            hi = hi.max(end);
        }
    }
    if lo >= hi { return; }

    bm.base = lo;
    bm.page_count = ((hi - lo) / PAGE_2M) as usize;
    assert!(bm.page_count <= MAX_PAGES, "pmm: physical memory exceeds {} GB", MAX_PAGES * 2 / 1024);

    // Mark usable pages as free (skip reserved regions).
    for entry in entries.iter().filter(|e| is_usable(e)) {
        let start = (entry.start + PAGE_2M - 1) & !(PAGE_2M - 1);
        let end = entry.end & !(PAGE_2M - 1);
        let mut addr = start;
        while addr + PAGE_2M <= end {
            if !overlaps_reserved(addr, addr + PAGE_2M, reserved) {
                let idx = bm.phys_to_idx(addr);
                bm.set_free(idx);
                bm.free_count += 1;
                bm.total_usable += 1;
            }
            addr += PAGE_2M;
        }
    }
}

/// Allocate one 2MB physical page. Does not heap-allocate (safe to call from the allocator).
pub fn alloc_page() -> Option<PhysPage> {
    let mut bm = BITMAP.lock();
    if bm.free_count == 0 { return None; }
    for idx in 0..bm.page_count {
        if bm.is_free(idx) {
            bm.set_used(idx);
            bm.free_count -= 1;
            let phys = bm.idx_to_phys(idx);
            drop(bm);
            unsafe {
                core::ptr::write_bytes(
                    DirectMap::from_phys(phys).as_mut_ptr::<u8>(), 0, PAGE_2M as usize,
                );
            }
            return Some(PhysPage(phys));
        }
    }
    None
}

/// Allocate `count` physically contiguous 2MB pages.
pub fn alloc_contiguous(count: usize) -> Option<alloc::vec::Vec<PhysPage>> {
    assert!(count > 0);
    let mut bm = BITMAP.lock();
    if bm.free_count < count { return None; }

    // Scan for a run of `count` consecutive free bits.
    let mut run = 0usize;
    let mut run_start = 0usize;
    for idx in 0..bm.page_count {
        if bm.is_free(idx) {
            if run == 0 { run_start = idx; }
            run += 1;
            if run == count {
                for i in run_start..run_start + count {
                    bm.set_used(i);
                }
                bm.free_count -= count;
                let base_phys = bm.idx_to_phys(run_start);
                drop(bm);

                let mut pages = alloc::vec::Vec::with_capacity(count);
                for i in 0..count {
                    let phys = base_phys + i as u64 * PAGE_2M;
                    unsafe {
                        core::ptr::write_bytes(
                            DirectMap::from_phys(phys).as_mut_ptr::<u8>(), 0, PAGE_2M as usize,
                        );
                    }
                    pages.push(PhysPage(phys));
                }
                return Some(pages);
            }
        } else {
            run = 0;
        }
    }
    None
}

/// Return a page to the bitmap (called by PhysPage::drop).
fn free_page(phys: u64) {
    let mut bm = BITMAP.lock();
    let idx = bm.phys_to_idx(phys);
    bm.set_free(idx);
    bm.free_count += 1;
}

/// Return (total_bytes, used_bytes).
pub fn stats() -> (u64, u64) {
    let bm = BITMAP.lock();
    let total = bm.total_usable as u64 * PAGE_2M;
    let used = (bm.total_usable - bm.free_count) as u64 * PAGE_2M;
    (total, used)
}

const EFI_LOADER_CODE: u32 = 1;
const EFI_LOADER_DATA: u32 = 2;
const EFI_BOOT_SERVICES_CODE: u32 = 3;
const EFI_BOOT_SERVICES_DATA: u32 = 4;
const EFI_CONVENTIONAL_MEMORY: u32 = 7;

fn is_usable(entry: &MemoryMapEntry) -> bool {
    matches!(entry.uefi_type,
        EFI_LOADER_CODE | EFI_LOADER_DATA |
        EFI_BOOT_SERVICES_CODE | EFI_BOOT_SERVICES_DATA |
        EFI_CONVENTIONAL_MEMORY)
}

fn overlaps_reserved(start: u64, end: u64, reserved: &[Region]) -> bool {
    reserved.iter().any(|r| start < r.end && end > r.start)
}
