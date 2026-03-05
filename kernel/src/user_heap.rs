// User-space heap allocator.
// Page-aligned chunks from the kernel allocator, mapped USER.
// Free regions tracked in a sorted Vec; first-fit alloc, merge-on-free.

use alloc::alloc::alloc_zeroed;
use alloc::vec::Vec;
use core::alloc::Layout;

use crate::arch::paging::{self, PAGE_2M};
use crate::log;

const CHUNK_SIZE: usize = 8 * PAGE_2M as usize; // 16 MB

pub struct UserHeap {
    /// Free regions available for allocation (sorted by start address).
    free: Vec<(u64, u64)>,
    /// All chunks allocated from the kernel (for validating free/realloc pointers).
    chunks: Vec<(u64, u64)>,
}

impl UserHeap {
    pub fn new() -> Self {
        Self { free: Vec::new(), chunks: Vec::new() }
    }

    /// Check that the entire range [addr, addr+size) falls within a known chunk.
    fn is_valid_range(&self, addr: u64, size: u64) -> bool {
        let Some(end) = addr.checked_add(size) else { return false };
        self.chunks.iter().any(|&(cs, ce)| addr >= cs && end <= ce)
    }
}

fn grow(heap: &mut UserHeap, min_size: usize) -> bool {
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
    heap.chunks.push((start, end));
    let pos = heap.free.iter().position(|&(s, _)| s > start).unwrap_or(heap.free.len());
    heap.free.insert(pos, (start, end));
    true
}

fn try_alloc(free: &mut Vec<(u64, u64)>, size: u64, align: u64) -> Option<u64> {
    for i in 0..free.len() {
        let (start, end) = free[i];
        let aligned = (start + align - 1) & !(align - 1);
        let alloc_end = aligned + size;

        if alloc_end <= end {
            if aligned > start && alloc_end < end {
                free[i] = (start, aligned);
                free.insert(i + 1, (alloc_end, end));
            } else if aligned > start {
                free[i] = (start, aligned);
            } else if alloc_end < end {
                free[i] = (alloc_end, end);
            } else {
                free.remove(i);
            }
            return Some(aligned);
        }
    }
    None
}

pub fn alloc(heap: &mut UserHeap, size: usize, align: usize) -> u64 {
    if size == 0 { return 0; }
    let align = align.max(1) as u64;
    let sz = size as u64;

    if let Some(addr) = try_alloc(&mut heap.free, sz, align) {
        return addr;
    }
    if !grow(heap, size + align as usize) {
        return 0;
    }
    try_alloc(&mut heap.free, sz, align).unwrap_or(0)
}

pub fn free(heap: &mut UserHeap, ptr: *mut u8, size: usize) {
    if ptr.is_null() || size == 0 { return; }
    let addr = ptr as u64;
    let Some(end) = addr.checked_add(size as u64) else { return };
    if !heap.is_valid_range(addr, size as u64) { return; }
    let pos = heap.free.iter().position(|&(s, _)| s > addr).unwrap_or(heap.free.len());
    heap.free.insert(pos, (addr, end));
    // Merge with next
    if pos + 1 < heap.free.len() && heap.free[pos].1 >= heap.free[pos + 1].0 {
        heap.free[pos].1 = heap.free[pos + 1].1;
        heap.free.remove(pos + 1);
    }
    // Merge with prev
    if pos > 0 && heap.free[pos - 1].1 >= heap.free[pos].0 {
        heap.free[pos - 1].1 = heap.free[pos].1;
        heap.free.remove(pos);
    }
}

pub fn realloc(heap: &mut UserHeap, ptr: *mut u8, size: usize, align: usize, new_size: usize) -> u64 {
    if ptr.is_null() {
        return alloc(heap, new_size, align);
    }
    if new_size <= size {
        return ptr as u64;
    }
    if !heap.is_valid_range(ptr as u64, size as u64) { return 0; }
    let new_ptr = alloc(heap, new_size, align);
    if new_ptr == 0 { return 0; }
    unsafe { core::ptr::copy_nonoverlapping(ptr, new_ptr as *mut u8, size); }
    free(heap, ptr, size);
    new_ptr
}
