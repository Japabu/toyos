use std::alloc::{self, Layout};
use std::ptr;

#[no_mangle]
pub unsafe extern "C" fn malloc(size: usize) -> *mut u8 {
    if size == 0 {
        return ptr::null_mut();
    }
    let layout = Layout::from_size_align(size + 16, 16).unwrap();
    let ptr = alloc::alloc(layout);
    if ptr.is_null() {
        return ptr;
    }
    // Store full layout size at start for free/realloc
    ptr::write(ptr as *mut usize, layout.size());
    ptr.add(16)
}

#[no_mangle]
pub unsafe extern "C" fn free(ptr: *mut u8) {
    if ptr.is_null() {
        return;
    }
    let real = ptr.sub(16);
    let total = ptr::read(real as *const usize);
    let layout = Layout::from_size_align_unchecked(total, 16);
    alloc::dealloc(real, layout);
}

#[no_mangle]
pub unsafe extern "C" fn calloc(count: usize, size: usize) -> *mut u8 {
    let total = count.wrapping_mul(size);
    let p = malloc(total);
    if !p.is_null() {
        ptr::write_bytes(p, 0, total);
    }
    p
}

#[no_mangle]
pub unsafe extern "C" fn realloc(ptr: *mut u8, new_size: usize) -> *mut u8 {
    if ptr.is_null() {
        return malloc(new_size);
    }
    if new_size == 0 {
        free(ptr);
        return ptr::null_mut();
    }
    let real = ptr.sub(16);
    let old_total = ptr::read(real as *const usize);
    let old_usable = old_total - 16;
    let new = malloc(new_size);
    if !new.is_null() {
        let copy_len = old_usable.min(new_size);
        ptr::copy_nonoverlapping(ptr, new, copy_len);
        free(ptr);
    }
    new
}

#[no_mangle]
pub unsafe extern "C" fn memcpy(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    ptr::copy_nonoverlapping(src, dst, n);
    dst
}

#[no_mangle]
pub unsafe extern "C" fn memmove(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    ptr::copy(src, dst, n);
    dst
}

#[no_mangle]
pub unsafe extern "C" fn memset(s: *mut u8, c: i32, n: usize) -> *mut u8 {
    ptr::write_bytes(s, c as u8, n);
    s
}

#[no_mangle]
pub unsafe extern "C" fn memcmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    for i in 0..n {
        let diff = *a.add(i) as i32 - *b.add(i) as i32;
        if diff != 0 {
            return diff;
        }
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn memchr(s: *const u8, c: i32, n: usize) -> *mut u8 {
    let c = c as u8;
    for i in 0..n {
        if *s.add(i) == c {
            return s.add(i) as *mut u8;
        }
    }
    ptr::null_mut()
}

#[inline(never)]
pub fn _libc_memory_init() {}
