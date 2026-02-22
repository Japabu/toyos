// User-space heap allocator.
// Page-aligned chunks from the kernel allocator, mapped USER.
// Free regions tracked in a sorted Vec; first-fit alloc, merge-on-free.

use alloc::alloc::{alloc_zeroed, Layout};
use alloc::vec::Vec;
use crate::arch::paging;
use crate::sync::SyncCell;

const CHUNK_SIZE: usize = 1024 * 1024; // 1MB

// Sorted list of free regions: (start, end)
static FREE_LIST: SyncCell<Vec<(u64, u64)>> = SyncCell::new(Vec::new());

/// Reset the user heap. Called before executing a new program.
pub fn init() {
    FREE_LIST.get_mut().clear();
    grow(CHUNK_SIZE);
}

/// Save current heap state (for nested exec).
pub fn save() -> Vec<(u64, u64)> {
    FREE_LIST.get_mut().clone()
}

/// Restore saved heap state.
pub fn restore(saved: Vec<(u64, u64)>) {
    *FREE_LIST.get_mut() = saved;
}

fn grow(min_size: usize) {
    let size = (min_size.max(CHUNK_SIZE) + 4095) & !4095;
    let layout = Layout::from_size_align(size, 4096).unwrap();
    let ptr = unsafe { alloc_zeroed(layout) };
    assert!(!ptr.is_null(), "user heap: out of memory");
    paging::map_user(ptr as u64, size as u64);
    let start = ptr as u64;
    let end = start + size as u64;
    let fl = FREE_LIST.get_mut();
    let pos = fl.iter().position(|&(s, _)| s > start).unwrap_or(fl.len());
    fl.insert(pos, (start, end));
}

/// First-fit search across free regions.
fn try_alloc(size: u64, align: u64) -> Option<u64> {
    let fl = FREE_LIST.get_mut();
    for i in 0..fl.len() {
        let (start, end) = fl[i];
        let aligned = (start + align - 1) & !(align - 1);
        let alloc_end = aligned + size;

        if alloc_end <= end {
            if aligned > start && alloc_end < end {
                fl[i] = (start, aligned);
                fl.insert(i + 1, (alloc_end, end));
            } else if aligned > start {
                fl[i] = (start, aligned);
            } else if alloc_end < end {
                fl[i] = (alloc_end, end);
            } else {
                fl.remove(i);
            }
            return Some(aligned);
        }
    }
    None
}

pub fn alloc(size: usize, align: usize) -> u64 {
    if size == 0 { return 0; }
    let align = align.max(1) as u64;
    let sz = size as u64;

    if let Some(addr) = try_alloc(sz, align) {
        return addr;
    }
    grow(size + align as usize);
    try_alloc(sz, align).expect("user heap: alloc failed after grow")
}

pub fn free(ptr: *mut u8, size: usize) {
    if ptr.is_null() || size == 0 { return; }
    let addr = ptr as u64;
    let end = addr + size as u64;
    let fl = FREE_LIST.get_mut();
    let pos = fl.iter().position(|&(s, _)| s > addr).unwrap_or(fl.len());
    fl.insert(pos, (addr, end));
    // Merge with next
    if pos + 1 < fl.len() && fl[pos].1 >= fl[pos + 1].0 {
        fl[pos].1 = fl[pos + 1].1;
        fl.remove(pos + 1);
    }
    // Merge with prev
    if pos > 0 && fl[pos - 1].1 >= fl[pos].0 {
        fl[pos - 1].1 = fl[pos].1;
        fl.remove(pos);
    }
}

pub fn realloc(ptr: *mut u8, size: usize, align: usize, new_size: usize) -> u64 {
    if ptr.is_null() {
        return alloc(new_size, align);
    }
    if new_size <= size {
        return ptr as u64;
    }
    let new_ptr = alloc(new_size, align);
    if new_ptr == 0 { return 0; }
    unsafe { core::ptr::copy_nonoverlapping(ptr, new_ptr as *mut u8, size); }
    free(ptr, size);
    new_ptr
}
