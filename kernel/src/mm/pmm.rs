use core::sync::atomic::{AtomicU64, Ordering};

use super::{DirectMap, PAGE_2M};
use crate::sync::Lock;
use crate::MemoryMapEntry;

/// Region of physical memory to exclude from the free list.
#[derive(Debug, Clone, Copy)]
pub struct Region {
    pub start: u64,
    pub end: u64,
}

// ---------------------------------------------------------------------------
// Allocation categories — every PMM allocation is tagged
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Category {
    KernelHeap   = 0,  // dlmalloc backing pages (global allocator)
    DemandPage   = 1,  // page fault handler
    Mmap         = 2,  // sys_mmap
    SharedMemory = 3,  // shared_memory::alloc
    Pipe         = 4,  // pipe ring buffers
    Elf          = 5,  // ELF loading (dlopen, cache, RW overlay)
    Tls          = 6,  // thread-local storage blocks
    Dma          = 7,  // DMA pools (drivers)
    Framebuffer  = 8,  // GPU framebuffers
    PageTable    = 9,  // paging::map_alloc (demand page mapping)
    Stack        = 10, // user stacks
    InitTls      = 11, // initial TLS block at spawn
}

const NUM_CATEGORIES: usize = 12;

impl Category {
    fn name(self) -> &'static str {
        match self {
            Category::KernelHeap   => "kernel-heap",
            Category::DemandPage   => "demand-page",
            Category::Mmap         => "mmap",
            Category::SharedMemory => "shared-mem",
            Category::Pipe         => "pipe",
            Category::Elf          => "elf",
            Category::Tls          => "tls",
            Category::Dma          => "dma",
            Category::Framebuffer  => "framebuffer",
            Category::PageTable    => "page-table",
            Category::Stack        => "stack",
            Category::InitTls      => "init-tls",
        }
    }
}

struct CategoryCounters {
    alloc_pages: AtomicU64,
    free_pages: AtomicU64,
}

impl CategoryCounters {
    const fn new() -> Self {
        Self {
            alloc_pages: AtomicU64::new(0),
            free_pages: AtomicU64::new(0),
        }
    }
}

static CATEGORY_STATS: [CategoryCounters; NUM_CATEGORIES] =
    [const { CategoryCounters::new() }; NUM_CATEGORIES];

/// Snapshot of the last time `dump_stats` ran, for computing rates.
static LAST_DUMP_NANOS: AtomicU64 = AtomicU64::new(0);
static LAST_ALLOC: [AtomicU64; NUM_CATEGORIES] = [const { AtomicU64::new(0) }; NUM_CATEGORIES];

/// Log per-category page allocation stats to serial.
pub fn dump_stats() {
    let now = crate::clock::nanos_since_boot();
    let prev = LAST_DUMP_NANOS.swap(now, Ordering::Relaxed);
    let dt_secs = if prev == 0 { 0.0 } else { (now - prev) as f64 / 1_000_000_000.0 };

    let (total, used) = stats();
    crate::log!("PMM: {}/{}MB used ({} pages free)",
        used / (1024 * 1024), total / (1024 * 1024),
        (total - used) / PAGE_2M);

    for i in 0..NUM_CATEGORIES {
        let alloc = CATEGORY_STATS[i].alloc_pages.load(Ordering::Relaxed);
        let free = CATEGORY_STATS[i].free_pages.load(Ordering::Relaxed);
        let held = alloc.saturating_sub(free);
        if alloc == 0 { continue; }

        let prev_alloc = LAST_ALLOC[i].swap(alloc, Ordering::Relaxed);
        let rate = if dt_secs > 0.0 {
            ((alloc - prev_alloc) as f64 / dt_secs) as u64
        } else {
            0
        };

        // Safety: i < NUM_CATEGORIES which equals the number of Category variants
        let cat = unsafe { core::mem::transmute::<u8, Category>(i as u8) };
        crate::log!("  {:12} alloc={:6} free={:6} held={:6} ({}MB) rate={}/s",
            cat.name(), alloc, free, held, held * 2, rate);
    }
}

/// Proof of ownership of one 2MB physical page. Non-Copy, non-Clone.
/// Drop returns the page to the free list and decrements category counters.
pub struct PhysPage {
    phys: u64,       // raw physical address, 2MB-aligned
    category: u8,    // Category as u8
}

impl PhysPage {
    /// Reconstruct from a raw physical address. Caller must ensure this
    /// is a valid 2MB-aligned page that was previously allocated.
    /// Assigned to KernelHeap category (used by dlmalloc lifetime management).
    pub(super) fn from_raw(phys: u64) -> Self {
        Self { phys, category: Category::KernelHeap as u8 }
    }

    /// Physical address (for page table entries, internal use only).
    pub(super) fn phys(&self) -> u64 {
        self.phys
    }

    /// Access this page through the kernel direct map.
    pub fn direct_map(&self) -> super::DirectMap {
        super::DirectMap::from_phys(self.phys)
    }

}

impl Drop for PhysPage {
    fn drop(&mut self) {
        let cat = self.category as usize;
        if cat < NUM_CATEGORIES {
            CATEGORY_STATS[cat].free_pages.fetch_add(1, Ordering::Relaxed);
        }
        free_page(self.phys);
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
    /// Hint for next free page scan — avoids re-scanning already-allocated prefix.
    next_hint: usize,
}

impl Bitmap {
    const fn new() -> Self {
        Self {
            bits: [0; MAX_PAGES / 64],
            base: 0,
            page_count: 0,
            free_count: 0,
            total_usable: 0,
            next_hint: 0,
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
pub fn alloc_page(cat: Category) -> Option<PhysPage> {
    let mut bm = BITMAP.lock();
    if bm.free_count == 0 { return None; }
    let start = bm.next_hint;
    for offset in 0..bm.page_count {
        let idx = (start + offset) % bm.page_count;
        if bm.is_free(idx) {
            bm.set_used(idx);
            bm.free_count -= 1;
            bm.next_hint = if idx + 1 < bm.page_count { idx + 1 } else { 0 };
            let phys = bm.idx_to_phys(idx);
            drop(bm);
            unsafe {
                core::ptr::write_bytes(
                    DirectMap::from_phys(phys).as_mut_ptr::<u8>(), 0, PAGE_2M as usize,
                );
            }
            CATEGORY_STATS[cat as usize].alloc_pages.fetch_add(1, Ordering::Relaxed);
            return Some(PhysPage { phys, category: cat as u8 });
        }
    }
    None
}

/// Allocate `count` physically contiguous 2MB pages.
pub fn alloc_contiguous(count: usize, cat: Category) -> Option<alloc::vec::Vec<PhysPage>> {
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

                CATEGORY_STATS[cat as usize].alloc_pages.fetch_add(count as u64, Ordering::Relaxed);
                let mut pages = alloc::vec::Vec::with_capacity(count);
                for i in 0..count {
                    let phys = base_phys + i as u64 * PAGE_2M;
                    unsafe {
                        core::ptr::write_bytes(
                            DirectMap::from_phys(phys).as_mut_ptr::<u8>(), 0, PAGE_2M as usize,
                        );
                    }
                    pages.push(PhysPage { phys, category: cat as u8 });
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
    assert!(!bm.is_free(idx), "double free of physical page at {:#x}", phys);
    bm.set_free(idx);
    bm.free_count += 1;
    bm.next_hint = bm.next_hint.min(idx);
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
