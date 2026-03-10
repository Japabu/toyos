use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr::null_mut;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::MemoryMapEntry;

// --- Public types ---

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub start: u64,
    pub end: u64,
}

// --- Constants ---

const PAGE_SIZE: usize = 4096;
const MAX_ORDER: usize = 18; // 2^18 pages = 1GB max block
const NO_PFN: u64 = u64::MAX;
const SLAB_CLASSES: usize = 9; // 8, 16, 32, 64, 128, 256, 512, 1024, 2048

// --- Phase 1: Early bump allocator (static buffer, used before paging) ---

const EARLY_SIZE: usize = 512 * 1024; // 512KB — enough for paging::init() page tables

#[repr(C, align(4096))]
struct EarlyBuffer([u8; EARLY_SIZE]);

/// Boot-only cell. Only accessed during single-threaded BSP boot (before SMP).
#[repr(transparent)]
struct BootCell<T>(UnsafeCell<T>);
unsafe impl<T> Sync for BootCell<T> {}
impl<T> BootCell<T> {
    const fn new(val: T) -> Self { Self(UnsafeCell::new(val)) }
    /// # Safety: only call during single-threaded boot phase.
    unsafe fn get(&self) -> *mut T { self.0.get() }
}

static EARLY_BUF: BootCell<EarlyBuffer> = BootCell::new(EarlyBuffer([0; EARLY_SIZE]));
static EARLY_POS: BootCell<usize> = BootCell::new(0);

// --- Phase 2: Buddy + Slab ---

#[repr(C)]
struct FreeBlock {
    next: u64,  // PFN of next free block, NO_PFN = end
    prev: u64,  // PFN of prev free block, NO_PFN = head sentinel
    order: u64, // order of this free block
}

struct BuddyAllocator {
    free_lists: [u64; MAX_ORDER + 1],
    bitmap: *mut u64,
    page_count: usize,
    free_pages: usize,
    total_usable_pages: usize,
}

impl BuddyAllocator {
    const fn new() -> Self {
        Self {
            free_lists: [NO_PFN; MAX_ORDER + 1],
            bitmap: null_mut(),
            page_count: 0,
            free_pages: 0,
            total_usable_pages: 0,
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
        (pfn as usize * PAGE_SIZE) as *mut FreeBlock
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

    fn alloc(&mut self, order: usize) -> *mut u8 {
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

        (pfn as usize * PAGE_SIZE) as *mut u8
    }

    fn free(&mut self, pfn: u64, order: usize) {
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
}

struct SlabAllocator {
    free_lists: [*mut u8; SLAB_CLASSES],
}

impl SlabAllocator {
    const fn new() -> Self {
        Self {
            free_lists: [null_mut(); SLAB_CLASSES],
        }
    }

    fn alloc(&mut self, class: usize, buddy: &mut BuddyAllocator) -> *mut u8 {
        let head = self.free_lists[class];
        if !head.is_null() {
            let next = unsafe { *(head as *const *mut u8) };
            self.free_lists[class] = next;
            return head;
        }

        let page = buddy.alloc(0);
        if page.is_null() {
            return null_mut();
        }

        let obj_size = 8usize << class;
        let count = PAGE_SIZE / obj_size;

        // Link objects [1..count) into free list, return object 0
        for i in (1..count).rev() {
            let obj = unsafe { page.add(i * obj_size) };
            unsafe { *(obj as *mut *mut u8) = self.free_lists[class]; }
            self.free_lists[class] = obj;
        }

        page
    }

    fn free(&mut self, ptr: *mut u8, class: usize) {
        unsafe { *(ptr as *mut *mut u8) = self.free_lists[class]; }
        self.free_lists[class] = ptr;
    }
}

// --- Combined allocator ---

const PHASE_UNINIT: u8 = 0;
const PHASE_EARLY: u8 = 1;
const PHASE_READY: u8 = 2;

struct KernelAllocator {
    ticket: AtomicU32,
    now: AtomicU32,
    buddy: UnsafeCell<BuddyAllocator>,
    slab: UnsafeCell<SlabAllocator>,
    phase: UnsafeCell<u8>,
}

unsafe impl Sync for KernelAllocator {}

impl KernelAllocator {
    const fn new() -> Self {
        Self {
            ticket: AtomicU32::new(0),
            now: AtomicU32::new(0),
            buddy: UnsafeCell::new(BuddyAllocator::new()),
            slab: UnsafeCell::new(SlabAllocator::new()),
            phase: UnsafeCell::new(PHASE_UNINIT),
        }
    }

    /// Acquire the allocator lock, disabling interrupts to prevent deadlock.
    /// Returns the saved RFLAGS for restoring interrupt state on release.
    fn acquire(&self) -> u64 {
        let rflags: u64;
        unsafe { core::arch::asm!("pushfq; pop {}", out(reg) rflags); }
        unsafe { core::arch::asm!("cli"); }
        let my_ticket = self.ticket.fetch_add(1, Ordering::Relaxed);
        let mut spins = 0u64;
        while self.now.load(Ordering::Acquire) != my_ticket {
            core::hint::spin_loop();
            spins += 1;
            if spins == 10_000_000 {
                // Deadlock detected — write directly to serial
                unsafe {
                    for &b in b"DEADLOCK ticket=" {
                        core::arch::asm!("out dx, al", in("dx") 0x3F8u16, in("al") b);
                    }
                    for i in (0..8).rev() {
                        let nibble = ((my_ticket >> (i * 4)) & 0xF) as u8;
                        let c = if nibble < 10 { b'0' + nibble } else { b'A' + nibble - 10 };
                        core::arch::asm!("out dx, al", in("dx") 0x3F8u16, in("al") c);
                    }
                    for &b in b" now=" {
                        core::arch::asm!("out dx, al", in("dx") 0x3F8u16, in("al") b);
                    }
                    let now_val = self.now.load(Ordering::Relaxed);
                    for i in (0..8).rev() {
                        let nibble = ((now_val >> (i * 4)) & 0xF) as u8;
                        let c = if nibble < 10 { b'0' + nibble } else { b'A' + nibble - 10 };
                        core::arch::asm!("out dx, al", in("dx") 0x3F8u16, in("al") c);
                    }
                    core::arch::asm!("out dx, al", in("dx") 0x3F8u16, in("al") b'\n');
                }
                spins = 0; // reset to print again
            }
        }
        rflags
    }

    fn release(&self, saved_rflags: u64) {
        self.now.fetch_add(1, Ordering::Release);
        if saved_rflags & 0x200 != 0 {
            unsafe { core::arch::asm!("sti"); }
        }
    }
}

fn size_class(size: usize) -> usize {
    if size <= 8 { return 0; }
    size.next_power_of_two().trailing_zeros() as usize - 3
}

fn order_for(size: usize, align: usize) -> usize {
    let pages = (size + PAGE_SIZE - 1) / PAGE_SIZE;
    let size_order = if pages <= 1 { 0 } else { pages.next_power_of_two().trailing_zeros() as usize };
    let align_order = if align <= PAGE_SIZE {
        0
    } else {
        (align / PAGE_SIZE).next_power_of_two().trailing_zeros() as usize
    };
    size_order.max(align_order)
}

unsafe fn early_alloc(layout: Layout) -> *mut u8 {
    let buf = EARLY_BUF.get().cast::<u8>();
    let pos = &mut *EARLY_POS.get();
    let aligned = (*pos + layout.align() - 1) & !(layout.align() - 1);
    let new_pos = aligned + layout.size();
    if new_pos > EARLY_SIZE {
        return null_mut();
    }
    *pos = new_pos;
    buf.add(aligned)
}

fn is_early_ptr(ptr: *mut u8) -> bool {
    let buf_start = EARLY_BUF.0.get() as usize;
    let p = ptr as usize;
    p >= buf_start && p < buf_start + EARLY_SIZE
}

unsafe impl GlobalAlloc for KernelAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let phase = *self.phase.get();
        if phase == PHASE_UNINIT {
            return null_mut();
        }
        if phase == PHASE_EARLY {
            // No lock needed — early phase is single-threaded (BSP only, before SMP)
            return early_alloc(layout);
        }

        let flags = self.acquire();
        let buddy = &mut *self.buddy.get();
        let slab = &mut *self.slab.get();
        let effective = layout.size().max(layout.align());
        let result = if effective <= 2048 {
            slab.alloc(size_class(effective), buddy)
        } else {
            buddy.alloc(order_for(layout.size(), layout.align()))
        };
        self.release(flags);
        result
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // Early allocations are permanent (page tables etc.) — never freed
        if is_early_ptr(ptr) {
            return;
        }

        let flags = self.acquire();
        let buddy = &mut *self.buddy.get();
        let slab = &mut *self.slab.get();
        let effective = layout.size().max(layout.align());
        if effective <= 2048 {
            slab.free(ptr, size_class(effective));
        } else {
            let order = order_for(layout.size(), layout.align());
            buddy.free(ptr as u64 / PAGE_SIZE as u64, order);
        }
        self.release(flags);
    }
}

#[global_allocator]
static ALLOCATOR: KernelAllocator = KernelAllocator::new();

// --- Initialization ---

fn is_usable_memory(entry: &MemoryMapEntry) -> bool {
    matches!(
        entry.uefi_type,
        1 | 2 | 3 | 4 | 7
    )
}

/// Phase 1: Enable the early bump allocator.
/// Called before paging::init(). Uses a static buffer — no writes to physical RAM.
pub unsafe fn init(
    _entries: &[MemoryMapEntry],
    _reserved_regions: &[Region],
) {
    *ALLOCATOR.phase.get() = PHASE_EARLY;
}

/// Phase 2: Switch to the buddy + slab allocator.
/// Called after paging::init() has set up identity mapping (all physical RAM writable).
pub unsafe fn init_buddy(
    entries: &[MemoryMapEntry],
    reserved_regions: &[Region],
) {
    // Reserve the UEFI memory region containing the current stack.
    // We're still on the UEFI firmware stack, which lives in usable memory.
    // Without this, build_free_lists would write intrusive FreeBlock headers
    // into stack pages (both below AND above RSP), corrupting return addresses.
    // Reserve the UEFI firmware stack. The initial RSP may span multiple memory
    // map entries (the entry containing RSP and entries above it for the stack
    // base/initial RSP). Reserve a generous 4MB below AND 1MB above current RSP
    // to cover the entire UEFI stack regardless of how memory map entries are split.
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

    let buddy = &mut *ALLOCATOR.buddy.get();

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
    buddy.bitmap = bitmap_addr as *mut u64;

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

    // Build buddy free lists
    build_free_lists(buddy);

    *ALLOCATOR.phase.get() = PHASE_READY;
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

        buddy.list_insert(pfn as u64, order);
        buddy.free_pages += 1 << order;
        pfn += 1 << order;
    }
}

/// Returns (total_usable_bytes, used_bytes).
pub fn memory_stats() -> (u64, u64) {
    let flags = ALLOCATOR.acquire();
    let buddy = unsafe { &*ALLOCATOR.buddy.get() };
    let total = buddy.total_usable_pages as u64 * PAGE_SIZE as u64;
    let used = (buddy.total_usable_pages - buddy.free_pages) as u64 * PAGE_SIZE as u64;
    ALLOCATOR.release(flags);
    (total, used)
}
