// Physical Memory Manager — buddy allocator for physical page frames.
//
// Manages all physical RAM. Provides alloc_pages(order)/free_pages(ptr, order)
// for page-granularity allocations. The kernel heap (slab allocator in
// allocator.rs) sits on top of this.

use core::ptr::null_mut;

use crate::MemoryMapEntry;

const PAGE_SIZE: usize = 4096;
const MAX_ORDER: usize = 18; // 2^18 pages = 1GB max block
const NO_PFN: u64 = u64::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub start: u64,
    pub end: u64,
}

#[repr(C)]
struct FreeBlock {
    next: u64,  // PFN of next free block, NO_PFN = end
    prev: u64,  // PFN of prev free block, NO_PFN = head sentinel
    order: u64, // order of this free block
}

pub(crate) struct BuddyAllocator {
    free_lists: [u64; MAX_ORDER + 1],
    bitmap: *mut u64,
    page_count: usize,
    free_pages: usize,
    total_usable_pages: usize,
    /// Reserved region PFN ranges — alloc asserts we never return these.
    reserved_ranges: [(usize, usize); 8], // (start_pfn, end_pfn)
    reserved_count: usize,
}

impl BuddyAllocator {
    pub(crate) const fn new() -> Self {
        Self {
            free_lists: [NO_PFN; MAX_ORDER + 1],
            bitmap: null_mut(),
            page_count: 0,
            free_pages: 0,
            total_usable_pages: 0,
            reserved_ranges: [(0, 0); 8],
            reserved_count: 0,
        }
    }

    fn is_allocated(&self, pfn: usize) -> bool {
        let word = pfn / 64;
        let bit = pfn % 64;
        unsafe { (*self.bitmap.add(word) >> bit) & 1 == 1 }
    }

    fn set_allocated(&self, pfn: usize) {
        let word = pfn / 64;
        let bit = pfn % 64;
        unsafe { *self.bitmap.add(word) |= 1u64 << bit; }
    }

    fn clear_allocated(&self, pfn: usize) {
        let word = pfn / 64;
        let bit = pfn % 64;
        unsafe { *self.bitmap.add(word) &= !(1u64 << bit); }
    }

    fn block_ptr(&self, pfn: u64) -> *mut FreeBlock {
        crate::PhysAddr::from_pfn(pfn).as_mut_ptr::<FreeBlock>()
    }

    fn list_insert(&mut self, pfn: u64, order: usize) {
        let old_head = self.free_lists[order];
        let block = self.block_ptr(pfn);
        unsafe {
            (*block).next = old_head;
            (*block).prev = NO_PFN;
            (*block).order = order as u64;
            if old_head != NO_PFN {
                (*self.block_ptr(old_head)).prev = pfn;
            }
        }
        self.free_lists[order] = pfn;
    }

    fn list_remove(&mut self, pfn: u64, order: usize) {
        let block = self.block_ptr(pfn);
        let (next, prev) = unsafe { ((*block).next, (*block).prev) };
        if prev != NO_PFN {
            unsafe { (*self.block_ptr(prev)).next = next; }
        } else {
            self.free_lists[order] = next;
        }
        if next != NO_PFN {
            unsafe { (*self.block_ptr(next)).prev = prev; }
        }
    }

    pub(crate) fn alloc(&mut self, order: usize) -> *mut u8 {
        // Find smallest available order >= requested
        let mut k = order;
        while k <= MAX_ORDER {
            if self.free_lists[k] != NO_PFN {
                break;
            }
            k += 1;
        }
        if k > MAX_ORDER {
            return null_mut();
        }

        let pfn = self.free_lists[k];
        self.list_remove(pfn, k);

        // Split down to requested order
        while k > order {
            k -= 1;
            let buddy_pfn = pfn + (1u64 << k);
            self.list_insert(buddy_pfn, k);
        }

        // Verify pages are NOT already allocated (catch double-alloc / corruption)
        let page_count = 1usize << order;
        for i in 0..page_count {
            if self.is_allocated(pfn as usize + i) {
                panic!(
                    "buddy alloc: page {} already allocated (pfn={:#x}, order={}, i={})",
                    pfn as usize + i, pfn, order, i
                );
            }
        }

        // Mark pages as allocated in bitmap
        for i in 0..page_count {
            self.set_allocated(pfn as usize + i);
        }
        self.free_pages -= page_count;

        // Assert we never return memory from reserved regions (kernel, initrd, etc.)
        let alloc_start = pfn as usize;
        let alloc_end = alloc_start + page_count;
        for i in 0..self.reserved_count {
            let (res_start, res_end) = self.reserved_ranges[i];
            if alloc_start < res_end && alloc_end > res_start {
                // Raw serial to avoid layout shift from format strings
                unsafe {
                    for &b in b"BUDDY ALLOC RESERVED!\n" {
                        core::arch::asm!("out dx, al", in("dx") 0x3F8u16, in("al") b);
                    }
                }
                crate::arch::cpu::halt();
            }
        }

        crate::PhysAddr::from_pfn(pfn).as_mut_ptr::<u8>()
    }

    pub(crate) fn free(&mut self, pfn: u64, order: usize) {
        let page_count = 1usize << order;

        // Verify pages ARE allocated (catch double-free)
        for i in 0..page_count {
            if !self.is_allocated(pfn as usize + i) {
                panic!(
                    "buddy free: page {} not allocated (pfn={:#x}, order={}, i={})",
                    pfn as usize + i, pfn, order, i
                );
            }
        }


        // Clear bitmap bits
        for i in 0..page_count {
            self.clear_allocated(pfn as usize + i);
        }
        self.free_pages += page_count;

        // Coalesce with buddies
        let mut pfn = pfn;
        let mut ord = order;
        while ord < MAX_ORDER {
            let buddy_pfn = pfn ^ (1u64 << ord);
            let buddy_end = buddy_pfn as usize + (1 << ord);

            if buddy_end > self.page_count {
                break;
            }

            // Check if all buddy pages are free
            let mut all_free = true;
            for i in 0..(1usize << ord) {
                if self.is_allocated(buddy_pfn as usize + i) {
                    all_free = false;
                    break;
                }
            }
            if !all_free {
                break;
            }

            // Verify buddy is a complete free block at this exact order
            let buddy_block = self.block_ptr(buddy_pfn);
            let buddy_order = unsafe { (*buddy_block).order };
            if buddy_order != ord as u64 {
                break;
            }

            self.list_remove(buddy_pfn, ord);
            pfn = pfn.min(buddy_pfn);
            ord += 1;
        }

        self.list_insert(pfn, ord);
    }

    pub(crate) fn memory_stats(&self) -> (u64, u64) {
        let total = self.total_usable_pages as u64 * PAGE_SIZE as u64;
        let used = (self.total_usable_pages - self.free_pages) as u64 * PAGE_SIZE as u64;
        (total, used)
    }
}

// --- Initialization ---

fn is_usable_memory(entry: &MemoryMapEntry) -> bool {
    matches!(entry.uefi_type, 1 | 2 | 3 | 4 | 7)
}

pub(crate) unsafe fn init_buddy(
    buddy: &mut BuddyAllocator,
    entries: &[MemoryMapEntry],
    reserved_regions: &[Region],
) {
    // Reserve the UEFI firmware stack.
    let rsp: u64;
    core::arch::asm!("mov {}, rsp", out(reg) rsp);
    let stack_bottom = (rsp.saturating_sub(4 * 1024 * 1024)) & !(PAGE_SIZE as u64 - 1);
    let stack_top = ((rsp + 1024 * 1024) + PAGE_SIZE as u64 - 1) & !(PAGE_SIZE as u64 - 1);
    let stack_region = Region { start: stack_bottom, end: stack_top };

    let mut all_reserved = [Region { start: 0, end: 0 }; 16];
    let n = reserved_regions.len();
    all_reserved[..n].copy_from_slice(reserved_regions);
    all_reserved[n] = stack_region;
    let reserved_regions = &all_reserved[..n + 1];

    // Find max physical address
    let max_addr = entries.iter()
        .filter(|e| is_usable_memory(e))
        .map(|e| e.end)
        .max()
        .expect("no usable memory");
    let page_count = max_addr as usize / PAGE_SIZE;
    buddy.page_count = page_count;

    // Compute bitmap size
    let bitmap_words = (page_count + 63) / 64;
    let bitmap_bytes = bitmap_words * 8;
    let bitmap_pages = (bitmap_bytes + PAGE_SIZE - 1) / PAGE_SIZE;

    // Find contiguous usable region for bitmap (skip reserved regions)
    let bitmap_addr = find_bitmap_placement(entries, reserved_regions, bitmap_pages)
        .expect("no space for allocator bitmap");
    buddy.bitmap = crate::PhysAddr::new(bitmap_addr as u64).as_mut_ptr::<u64>();

    // Initialize bitmap: all bits = 1 (everything allocated)
    let bitmap_slice = core::slice::from_raw_parts_mut(buddy.bitmap, bitmap_words);
    for word in bitmap_slice.iter_mut() {
        *word = u64::MAX;
    }

    // Walk usable entries, clear bits for usable pages
    let mut total_usable = 0usize;
    for entry in entries.iter().filter(|e| is_usable_memory(e)) {
        let start_pfn = entry.start as usize / PAGE_SIZE;
        let end_pfn = entry.end as usize / PAGE_SIZE;
        for pfn in start_pfn..end_pfn.min(page_count) {
            buddy.clear_allocated(pfn);
            total_usable += 1;
        }
    }

    // Re-set bits for reserved regions
    for region in reserved_regions {
        let start_pfn = region.start as usize / PAGE_SIZE;
        let end_pfn = (region.end as usize + PAGE_SIZE - 1) / PAGE_SIZE;
        for pfn in start_pfn..end_pfn.min(page_count) {
            if !buddy.is_allocated(pfn) {
                buddy.set_allocated(pfn);
                total_usable -= 1;
            }
        }
    }

    // Re-set bits for bitmap's own pages
    let bitmap_start_pfn = bitmap_addr as usize / PAGE_SIZE;
    for pfn in bitmap_start_pfn..bitmap_start_pfn + bitmap_pages {
        if !buddy.is_allocated(pfn) {
            buddy.set_allocated(pfn);
            total_usable -= 1;
        }
    }

    // Re-set page 0 (null safety)
    if page_count > 0 && !buddy.is_allocated(0) {
        buddy.set_allocated(0);
        total_usable -= 1;
    }

    buddy.total_usable_pages = total_usable;
    buddy.free_pages = 0;

    // Store reserved ranges for runtime assertions in alloc()
    let mut rc = 0;
    for region in reserved_regions {
        if region.start < region.end && rc < buddy.reserved_ranges.len() {
            let start_pfn = region.start as usize / PAGE_SIZE;
            let end_pfn = (region.end as usize + PAGE_SIZE - 1) / PAGE_SIZE;
            buddy.reserved_ranges[rc] = (start_pfn, end_pfn);
            rc += 1;
        }
    }
    // Also reserve the bitmap's own pages
    if rc < buddy.reserved_ranges.len() {
        buddy.reserved_ranges[rc] = (bitmap_start_pfn, bitmap_start_pfn + bitmap_pages);
        rc += 1;
    }
    buddy.reserved_count = rc;

    // Build buddy free lists
    build_free_lists(buddy);
}

fn find_bitmap_placement(
    entries: &[MemoryMapEntry],
    reserved: &[Region],
    bitmap_pages: usize,
) -> Option<u64> {
    let bitmap_bytes = bitmap_pages * PAGE_SIZE;

    for entry in entries.iter().filter(|e| is_usable_memory(e)) {
        let mut cursor = entry.start;
        cursor = (cursor + PAGE_SIZE as u64 - 1) & !(PAGE_SIZE as u64 - 1);
        if cursor == 0 { cursor = PAGE_SIZE as u64; }

        while cursor + bitmap_bytes as u64 <= entry.end {
            let cand_end = cursor + bitmap_bytes as u64;
            let mut overlaps = false;
            let mut skip_to = cand_end;
            for r in reserved {
                if r.start < cand_end && r.end > cursor {
                    overlaps = true;
                    skip_to = (r.end + PAGE_SIZE as u64 - 1) & !(PAGE_SIZE as u64 - 1);
                    break;
                }
            }
            if !overlaps {
                return Some(cursor);
            }
            cursor = skip_to;
        }
    }
    None
}

fn build_free_lists(buddy: &mut BuddyAllocator) {
    let mut pfn = 0usize;
    let page_count = buddy.page_count;

    while pfn < page_count {
        if buddy.is_allocated(pfn) {
            pfn += 1;
            continue;
        }

        let mut order = 0usize;
        while order < MAX_ORDER {
            let block_size = 1usize << (order + 1);

            if pfn & (block_size - 1) != 0 {
                break;
            }

            let upper_end = pfn + block_size;
            if upper_end > page_count {
                break;
            }

            let mut all_free = true;
            let upper_start = pfn + (1 << order);
            for p in upper_start..upper_end {
                if buddy.is_allocated(p) {
                    all_free = false;
                    break;
                }
            }
            if !all_free {
                break;
            }

            order += 1;
        }

        // Skip poisoning during init — too slow for millions of pages.
        // Pages will be poisoned on their first free after allocation.
        buddy.list_insert(pfn as u64, order);
        buddy.free_pages += 1 << order;
        pfn += 1 << order;
    }
}
