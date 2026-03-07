use core::ptr;

// ToyOS: Call toyos-abi's kernel allocator directly.
// We store the allocation size in a header so C's free(ptr) can reconstruct it.
mod backend {
    const HEADER: usize = 16; // 16 for alignment

    pub unsafe fn alloc(size: usize) -> *mut u8 {
        let total = HEADER + size;
        // SAFETY: total > 0 (HEADER is 16), alignment is valid
        let raw = unsafe { toyos_abi::syscall::alloc(total, 16) };
        if raw.is_null() {
            return raw;
        }
        unsafe { *(raw as *mut usize) = size; }
        unsafe { raw.add(HEADER) }
    }

    pub unsafe fn dealloc(ptr: *mut u8) {
        let raw = unsafe { ptr.sub(HEADER) };
        let size = unsafe { *(raw as *const usize) };
        let total = HEADER + size;
        // SAFETY: raw was returned by alloc with the same total size and alignment
        unsafe { toyos_abi::syscall::free(raw, total, 16) };
    }

    pub unsafe fn realloc(ptr: *mut u8, new_size: usize) -> *mut u8 {
        let raw = unsafe { ptr.sub(HEADER) };
        let old_size = unsafe { *(raw as *const usize) };
        let old_total = HEADER + old_size;
        let new_total = HEADER + new_size;
        // SAFETY: raw was returned by alloc with old_total size and alignment 16
        let new_raw = unsafe { toyos_abi::syscall::realloc(raw, old_total, 16, new_total) };
        if new_raw.is_null() {
            return new_raw;
        }
        unsafe { *(new_raw as *mut usize) = new_size; }
        unsafe { new_raw.add(HEADER) }
    }
}

// --- C standard memory functions ---

#[no_mangle]
pub unsafe extern "C" fn malloc(size: usize) -> *mut u8 {
    if size == 0 {
        return ptr::null_mut();
    }
    unsafe { backend::alloc(size) }
}

#[no_mangle]
pub unsafe extern "C" fn free(p: *mut u8) {
    if p.is_null() {
        return;
    }
    unsafe { backend::dealloc(p); }
}

#[no_mangle]
pub unsafe extern "C" fn calloc(count: usize, size: usize) -> *mut u8 {
    let total = match count.checked_mul(size) {
        Some(t) => t,
        None => return ptr::null_mut(),
    };
    let p = unsafe { malloc(total) };
    if !p.is_null() {
        unsafe { ptr::write_bytes(p, 0, total); }
    }
    p
}

#[no_mangle]
pub unsafe extern "C" fn realloc(p: *mut u8, new_size: usize) -> *mut u8 {
    if p.is_null() {
        return unsafe { malloc(new_size) };
    }
    if new_size == 0 {
        unsafe { free(p); }
        return ptr::null_mut();
    }
    unsafe { backend::realloc(p, new_size) }
}

// memcpy, memmove, memset, memcmp — implemented in inline asm to avoid
// infinite recursion (Rust's ptr::copy_nonoverlapping emits calls to memcpy).

#[no_mangle]
pub unsafe extern "C" fn memcpy(dest: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    core::arch::asm!(
        "rep movsb",
        inout("rdi") dest => _,
        inout("rsi") src => _,
        inout("rcx") n => _,
        options(nostack),
    );
    dest
}

#[no_mangle]
pub unsafe extern "C" fn memmove(dest: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    if (dest as usize) <= (src as usize) || (dest as usize) >= (src as usize) + n {
        memcpy(dest, src, n);
    } else {
        // Overlap with dest after src — copy backwards
        core::arch::asm!(
            "std",
            "rep movsb",
            "cld",
            inout("rdi") dest.add(n - 1) => _,
            inout("rsi") src.add(n - 1) => _,
            inout("rcx") n => _,
            options(nostack),
        );
    }
    dest
}

#[no_mangle]
pub unsafe extern "C" fn memset(dest: *mut u8, c: i32, n: usize) -> *mut u8 {
    core::arch::asm!(
        "rep stosb",
        inout("rdi") dest => _,
        in("al") c as u8,
        inout("rcx") n => _,
        options(nostack),
    );
    dest
}

#[no_mangle]
pub unsafe extern "C" fn memcmp(s1: *const u8, s2: *const u8, n: usize) -> i32 {
    for i in 0..n {
        let a = *s1.add(i);
        let b = *s2.add(i);
        if a != b {
            return a as i32 - b as i32;
        }
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn memchr(s: *const u8, c: i32, n: usize) -> *mut u8 {
    let c = c as u8;
    for i in 0..n {
        if unsafe { *s.add(i) } == c {
            return unsafe { s.add(i) as *mut u8 };
        }
    }
    ptr::null_mut()
}