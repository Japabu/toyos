// User-space heap allocator.
// Page-aligned chunks from the kernel allocator, mapped USER.
// Free regions tracked in a sorted Vec; first-fit alloc, merge-on-free.

use alloc::alloc::alloc_zeroed;
use alloc::vec::Vec;
use core::alloc::Layout;

use crate::arch::paging::{self, PAGE_2M};
use crate::log;

const CHUNK_SIZE: usize = 8 * PAGE_2M as usize; // 16 MB

/// Create an initial user heap for a new process.
/// Returns empty — first SYS_ALLOC triggers grow in the correct CR3 context.
pub fn new_heap() -> Vec<(u64, u64)> {
    Vec::new()
}

fn grow(heap: &mut Vec<(u64, u64)>, min_size: usize) -> bool {
    let size = paging::align_2m(min_size.max(CHUNK_SIZE));
    let layout = Layout::from_size_align(size, PAGE_2M as usize).unwrap();
    let ptr = unsafe { alloc_zeroed(layout) };
    if ptr.is_null() {
        log!("user_heap: out of memory (requested {} KB)", size / 1024);
        return false;
    }
    let start = ptr as u64;
    let end = start + size as u64;
    log!("user_heap: grow {:#x}..{:#x} ({} KB)", start, end, size / 1024);
    paging::map_user(start, size as u64);
    let pos = heap.iter().position(|&(s, _)| s > start).unwrap_or(heap.len());
    heap.insert(pos, (start, end));
    true
}

fn try_alloc(heap: &mut Vec<(u64, u64)>, size: u64, align: u64) -> Option<u64> {
    for i in 0..heap.len() {
        let (start, end) = heap[i];
        let aligned = (start + align - 1) & !(align - 1);
        let alloc_end = aligned + size;

        if alloc_end <= end {
            if aligned > start && alloc_end < end {
                heap[i] = (start, aligned);
                heap.insert(i + 1, (alloc_end, end));
            } else if aligned > start {
                heap[i] = (start, aligned);
            } else if alloc_end < end {
                heap[i] = (alloc_end, end);
            } else {
                heap.remove(i);
            }
            return Some(aligned);
        }
    }
    None
}

pub fn alloc(heap: &mut Vec<(u64, u64)>, size: usize, align: usize) -> u64 {
    if size == 0 { return 0; }
    let align = align.max(1) as u64;
    let sz = size as u64;

    if let Some(addr) = try_alloc(heap, sz, align) {
        return addr;
    }
    if !grow(heap, size + align as usize) {
        return 0;
    }
    try_alloc(heap, sz, align).unwrap_or(0)
}

pub fn free(heap: &mut Vec<(u64, u64)>, ptr: *mut u8, size: usize) {
    if ptr.is_null() || size == 0 { return; }
    let addr = ptr as u64;
    let end = addr + size as u64;
    let pos = heap.iter().position(|&(s, _)| s > addr).unwrap_or(heap.len());
    heap.insert(pos, (addr, end));
    // Merge with next
    if pos + 1 < heap.len() && heap[pos].1 >= heap[pos + 1].0 {
        heap[pos].1 = heap[pos + 1].1;
        heap.remove(pos + 1);
    }
    // Merge with prev
    if pos > 0 && heap[pos - 1].1 >= heap[pos].0 {
        heap[pos - 1].1 = heap[pos].1;
        heap.remove(pos);
    }
}

pub fn realloc(heap: &mut Vec<(u64, u64)>, ptr: *mut u8, size: usize, align: usize, new_size: usize) -> u64 {
    if ptr.is_null() {
        return alloc(heap, new_size, align);
    }
    if new_size <= size {
        return ptr as u64;
    }
    let new_ptr = alloc(heap, new_size, align);
    if new_ptr == 0 { return 0; }
    unsafe { core::ptr::copy_nonoverlapping(ptr, new_ptr as *mut u8, size); }
    free(heap, ptr, size);
    new_ptr
}
