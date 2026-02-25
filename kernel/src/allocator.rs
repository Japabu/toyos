use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr::{null_mut, NonNull};
use core::sync::atomic::{AtomicU32, Ordering};

use alloc::vec::Vec;

use crate::MemoryMapEntry;

// --- Arena allocator (bootstrap only) ---

const ARENA_SIZE: usize = 128 * 1024;

#[repr(C, align(4096))]
struct Arena {
    buf: UnsafeCell<[u8; ARENA_SIZE]>,
    pos: UnsafeCell<usize>,
}

unsafe impl Sync for Arena {}

impl Arena {
    const fn new() -> Self {
        Self {
            buf: UnsafeCell::new([0; ARENA_SIZE]),
            pos: UnsafeCell::new(0),
        }
    }
}

unsafe impl core::alloc::Allocator for &'static Arena {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, core::alloc::AllocError> {
        unsafe {
            let pos = &mut *self.pos.get();
            let buf = (*self.buf.get()).as_mut_ptr();
            let aligned = (*pos + layout.align() - 1) & !(layout.align() - 1);
            let new_pos = aligned + layout.size();
            if new_pos > ARENA_SIZE {
                return Err(core::alloc::AllocError);
            }
            *pos = new_pos;
            let ptr = buf.add(aligned);
            Ok(NonNull::new_unchecked(core::ptr::slice_from_raw_parts_mut(ptr, layout.size())))
        }
    }

    unsafe fn deallocate(&self, _ptr: NonNull<u8>, _layout: Layout) {}
}

static ARENA: Arena = Arena::new();

// --- Region tracking ---

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub start: u64,
    pub end: u64,
}

fn is_usable_memory(entry: &MemoryMapEntry) -> bool {
    matches!(
        entry.uefi_type,
        1 // EfiLoaderCode
        | 2 // EfiLoaderData
        | 3 // EfiBootServicesCode
        | 4 // EfiBootServicesData
        | 7 // EfiConventionalMemory
    )
}

fn build_usable_regions(entries: &[MemoryMapEntry]) -> Vec<Region, &'static Arena> {
    let mut regions: Vec<Region, &'static Arena> = Vec::new_in(&ARENA);

    for entry in entries.iter().filter(|e| is_usable_memory(e)) {
        regions.push(Region { start: entry.start, end: entry.end });
    }

    regions.sort_unstable_by_key(|r| r.start);

    let mut merged: Vec<Region, &'static Arena> = Vec::new_in(&ARENA);
    for r in regions.iter() {
        if let Some(last) = merged.last_mut() {
            if r.start <= last.end {
                last.end = last.end.max(r.end);
                continue;
            }
        }
        merged.push(*r);
    }

    merged
}

// --- Real allocator with internal spinlock ---

struct RealAllocator {
    ticket: AtomicU32,
    now: AtomicU32,
    usable: UnsafeCell<Vec<Region, &'static Arena>>,
    reserved: UnsafeCell<Vec<Region, &'static Arena>>,
}

unsafe impl Sync for RealAllocator {}

impl RealAllocator {
    const fn new() -> Self {
        Self {
            ticket: AtomicU32::new(0),
            now: AtomicU32::new(0),
            usable: UnsafeCell::new(Vec::new_in(&ARENA)),
            reserved: UnsafeCell::new(Vec::new_in(&ARENA)),
        }
    }

    fn acquire(&self) {
        let my_ticket = self.ticket.fetch_add(1, Ordering::Relaxed);
        while self.now.load(Ordering::Acquire) != my_ticket {
            core::hint::spin_loop();
        }
    }

    fn release(&self) {
        self.now.fetch_add(1, Ordering::Release);
    }

    unsafe fn alloc_inner(&self, layout: Layout) -> *mut u8 {
        let usable = &*self.usable.get();
        let reserved = &mut *self.reserved.get();
        let size = layout.size() as u64;
        let align = layout.align() as u64;

        for u in usable.iter() {
            let mut cursor = u.start;

            for r in reserved.iter() {
                if r.start >= u.end { break; }
                if r.end <= cursor { continue; }

                if r.start > cursor {
                    let aligned_start = align_up(cursor, align);
                    let alloc_end = aligned_start + size;
                    if alloc_end <= r.start && alloc_end <= u.end {
                        sorted_insert(reserved, Region { start: aligned_start, end: alloc_end });
                        merge_in_place(reserved);
                        return aligned_start as *mut u8;
                    }
                }

                cursor = cursor.max(r.end);
            }

            if cursor < u.end {
                let aligned_start = align_up(cursor, align);
                let alloc_end = aligned_start + size;
                if alloc_end <= u.end {
                    sorted_insert(reserved, Region { start: aligned_start, end: alloc_end });
                    merge_in_place(reserved);
                    return aligned_start as *mut u8;
                }
            }
        }

        null_mut()
    }

    unsafe fn dealloc_inner(&self, ptr: *mut u8, layout: Layout) {
        let reserved = &mut *self.reserved.get();
        let free_start = ptr as u64;
        let free_end = free_start + layout.size() as u64;

        for i in 0..reserved.len() {
            let r = reserved[i];
            if r.start > free_start { break; }
            if r.start <= free_start && free_end <= r.end {
                if free_start == r.start && free_end == r.end {
                    reserved.remove(i);
                } else if free_start == r.start {
                    reserved[i].start = free_end;
                } else if free_end == r.end {
                    reserved[i].end = free_start;
                } else {
                    reserved[i].end = free_start;
                    sorted_insert(reserved, Region { start: free_end, end: r.end });
                }
                return;
            }
        }
    }
}

fn align_up(val: u64, align: u64) -> u64 {
    (val + align - 1) & !(align - 1)
}

fn merge_in_place(v: &mut Vec<Region, &'static Arena>) {
    if v.len() < 2 { return; }
    let mut write = 0;
    for read in 1..v.len() {
        if v[read].start <= v[write].end {
            v[write].end = v[write].end.max(v[read].end);
        } else {
            write += 1;
            v[write] = v[read];
        }
    }
    v.truncate(write + 1);
}

fn sorted_insert(v: &mut Vec<Region, &'static Arena>, region: Region) {
    let pos = v.iter().position(|r| r.start > region.start).unwrap_or(v.len());
    v.insert(pos, region);
}

unsafe impl GlobalAlloc for RealAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.acquire();
        let result = self.alloc_inner(layout);
        self.release();
        result
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        self.acquire();
        self.dealloc_inner(ptr, layout);
        self.release();
    }
}

#[global_allocator]
static ALLOCATOR: RealAllocator = RealAllocator::new();

/// Initialize the allocator. Must be called before any allocations.
/// Uses the arena internally to bootstrap the region vecs.
pub unsafe fn init(
    entries: &[MemoryMapEntry],
    reserved_regions: &[Region],
) {
    let usable = build_usable_regions(entries);

    let u = &mut *ALLOCATOR.usable.get();
    let r = &mut *ALLOCATOR.reserved.get();
    *u = usable;

    r.push(Region { start: 0, end: 0x1000 });
    for region in reserved_regions {
        r.push(*region);
    }

    r.sort_unstable_by_key(|r| r.start);
    merge_in_place(r);
}
