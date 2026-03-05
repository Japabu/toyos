use std::ptr;

// --- Platform-specific allocator backends ---

// macOS: Use the zone allocator directly to avoid infinite recursion.
// Our `malloc` symbol overrides the system `malloc`, so `std::alloc::alloc`
// → system allocator → our `malloc` → infinite loop.
// `malloc_zone_malloc`/`malloc_zone_free` bypass the `malloc` symbol entirely.
#[cfg(target_os = "macos")]
mod backend {
    extern "C" {
        fn malloc_default_zone() -> *mut u8;
        fn malloc_zone_malloc(zone: *mut u8, size: usize) -> *mut u8;
        fn malloc_zone_free(zone: *mut u8, ptr: *mut u8);
        fn malloc_zone_realloc(zone: *mut u8, ptr: *mut u8, size: usize) -> *mut u8;
    }

    unsafe fn zone() -> *mut u8 {
        unsafe { malloc_default_zone() }
    }

    pub unsafe fn alloc(size: usize) -> *mut u8 {
        unsafe { malloc_zone_malloc(zone(), size) }
    }

    pub unsafe fn dealloc(ptr: *mut u8) {
        unsafe { malloc_zone_free(zone(), ptr) }
    }

    pub unsafe fn realloc(ptr: *mut u8, new_size: usize) -> *mut u8 {
        unsafe { malloc_zone_realloc(zone(), ptr, new_size) }
    }
}

// ToyOS: Use std::alloc (the global allocator), which goes through the kernel
// syscall-based allocator — no risk of recursion since there's no symbol override.
#[cfg(target_os = "toyos")]
mod backend {
    use std::alloc::{self, Layout};

    // We store the allocation size in the 8 bytes before the returned pointer
    // so we can reconstruct the Layout for dealloc/realloc.
    const HEADER: usize = 16; // 16 for alignment

    pub unsafe fn alloc(size: usize) -> *mut u8 {
        let total = HEADER + size;
        let layout = Layout::from_size_align_unchecked(total, 16);
        let raw = unsafe { alloc::alloc(layout) };
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
        let layout = unsafe { Layout::from_size_align_unchecked(total, 16) };
        unsafe { alloc::dealloc(raw, layout); }
    }

    pub unsafe fn realloc(ptr: *mut u8, new_size: usize) -> *mut u8 {
        let raw = unsafe { ptr.sub(HEADER) };
        let old_size = unsafe { *(raw as *const usize) };
        let old_total = HEADER + old_size;
        let new_total = HEADER + new_size;
        let old_layout = unsafe { Layout::from_size_align_unchecked(old_total, 16) };
        let new_raw = unsafe { alloc::realloc(raw, old_layout, new_total) };
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

// memcpy, memmove, memset, memcmp are provided by compiler-builtins
// (rust/library/compiler-builtins/compiler-builtins/src/mem/x86_64.rs)
// using optimized rep movsb/movsq inline asm. Don't redefine them here
// or ptr::copy_nonoverlapping will call our memcpy which calls
// ptr::copy_nonoverlapping — infinite recursion.

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

// mmap/munmap stubs — ToyOS doesn't have mmap, but some parts of std reference it.
// These are no-ops that return failure, which is safe since the actual allocator
// goes through the kernel's alloc syscall, not mmap.
#[cfg(target_os = "toyos")]
#[no_mangle]
pub unsafe extern "C" fn mmap(
    _addr: *mut u8,
    _len: usize,
    _prot: i32,
    _flags: i32,
    _fd: i32,
    _offset: i64,
) -> *mut u8 {
    // MAP_FAILED = (void*)-1
    usize::MAX as *mut u8
}

#[cfg(target_os = "toyos")]
#[no_mangle]
pub unsafe extern "C" fn munmap(_addr: *mut u8, _len: usize) -> i32 {
    -1
}

#[inline(never)]
pub fn _libc_memory_init() {}
