use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr::{null_mut, NonNull};

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

// --- Real allocator ---

struct RealAllocator {
    usable: UnsafeCell<Vec<Region, &'static Arena>>,
    reserved: UnsafeCell<Vec<Region, &'static Arena>>,
}

unsafe impl Sync for RealAllocator {}

impl RealAllocator {
    const fn new() -> Self {
        Self {
            usable: UnsafeCell::new(Vec::new_in(&ARENA)),
            reserved: UnsafeCell::new(Vec::new_in(&ARENA)),
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

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
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

#[cfg(not(test))]
#[global_allocator]
static ALLOCATOR: RealAllocator = RealAllocator::new();

#[cfg(not(test))]
/// Initialize the allocator. Must be called before any allocations.
/// Uses the arena internally to bootstrap the region vecs.
pub unsafe fn init(entries: &[MemoryMapEntry], kernel_start: u64, kernel_size: u64) {
    let usable = build_usable_regions(entries);

    let u = &mut *ALLOCATOR.usable.get();
    let r = &mut *ALLOCATOR.reserved.get();
    *u = usable;

    r.push(Region { start: 0, end: 0x1000 });
    r.push(Region { start: kernel_start, end: kernel_start + kernel_size });

    r.sort_unstable_by_key(|r| r.start);
    merge_in_place(r);
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::alloc::GlobalAlloc;

    fn make_allocator(usable: &[Region]) -> RealAllocator {
        let alloc = RealAllocator::new();
        unsafe {
            let u = &mut *alloc.usable.get();
            for r in usable {
                u.push(*r);
            }
        }
        alloc
    }

    fn reserved_regions(alloc: &RealAllocator) -> Vec<Region> {
        unsafe { (*alloc.reserved.get()).iter().copied().collect() }
    }

    #[test]
    fn basic_alloc() {
        let alloc = make_allocator(&[Region { start: 0x1000, end: 0x10000 }]);
        unsafe {
            let ptr = alloc.alloc(Layout::from_size_align(4096, 1).unwrap());
            assert!(!ptr.is_null());
            assert_eq!(ptr as u64, 0x1000);
        }
    }

    #[test]
    fn aligned_alloc() {
        let alloc = make_allocator(&[Region { start: 0x1000, end: 0x10000 }]);
        unsafe {
            let ptr = alloc.alloc(Layout::from_size_align(4096, 4096).unwrap());
            assert!(!ptr.is_null());
            assert_eq!(ptr as u64 % 4096, 0);
        }
    }

    #[test]
    fn multiple_allocs_no_overlap() {
        let alloc = make_allocator(&[Region { start: 0x1000, end: 0x10000 }]);
        unsafe {
            let a = alloc.alloc(Layout::from_size_align(1024, 1).unwrap());
            let b = alloc.alloc(Layout::from_size_align(2048, 1).unwrap());
            let c = alloc.alloc(Layout::from_size_align(512, 1).unwrap());
            assert!(!a.is_null() && !b.is_null() && !c.is_null());

            let a = a as u64;
            let b = b as u64;
            let c = c as u64;
            // No overlaps
            assert!(a + 1024 <= b);
            assert!(b + 2048 <= c);
        }
    }

    #[test]
    fn dealloc_frees_region() {
        let alloc = make_allocator(&[Region { start: 0x1000, end: 0x10000 }]);
        let layout = Layout::from_size_align(4096, 1).unwrap();
        unsafe {
            let ptr = alloc.alloc(layout);
            assert!(!ptr.is_null());
            assert_eq!(reserved_regions(&alloc).len(), 1);

            alloc.dealloc(ptr, layout);
            assert_eq!(reserved_regions(&alloc).len(), 0);
        }
    }

    #[test]
    fn reuse_after_free() {
        let alloc = make_allocator(&[Region { start: 0x1000, end: 0x2000 }]);
        let layout = Layout::from_size_align(4096, 1).unwrap();
        unsafe {
            let ptr1 = alloc.alloc(layout);
            assert!(!ptr1.is_null());

            // Region is full
            let ptr2 = alloc.alloc(layout);
            assert!(ptr2.is_null());

            // Free and re-alloc
            alloc.dealloc(ptr1, layout);
            let ptr3 = alloc.alloc(layout);
            assert_eq!(ptr3 as u64, ptr1 as u64);
        }
    }

    #[test]
    fn alloc_fails_when_full() {
        let alloc = make_allocator(&[Region { start: 0x1000, end: 0x1100 }]);
        unsafe {
            let ptr = alloc.alloc(Layout::from_size_align(0x200, 1).unwrap());
            assert!(ptr.is_null());
        }
    }

    #[test]
    fn build_usable_regions_merges() {
        let entries = [
            MemoryMapEntry { uefi_type: 7, start: 0x1000, end: 0x3000 },
            MemoryMapEntry { uefi_type: 7, start: 0x2000, end: 0x5000 },
            MemoryMapEntry { uefi_type: 9, start: 0x6000, end: 0x7000 }, // not usable
        ];
        let regions = build_usable_regions(&entries);
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0], Region { start: 0x1000, end: 0x5000 });
    }

    #[test]
    fn dealloc_splits_region() {
        let alloc = make_allocator(&[Region { start: 0x1000, end: 0x10000 }]);
        let layout = Layout::from_size_align(0x1000, 1).unwrap();
        unsafe {
            let a = alloc.alloc(layout);
            let b = alloc.alloc(layout);
            let c = alloc.alloc(layout);
            assert!(!a.is_null() && !b.is_null() && !c.is_null());

            // Free middle allocation
            alloc.dealloc(b, layout);
            let reserved = reserved_regions(&alloc);
            assert_eq!(reserved.len(), 2);

            // Next alloc should reuse the freed middle slot
            let d = alloc.alloc(layout);
            assert_eq!(d as u64, b as u64);
        }
    }
}
