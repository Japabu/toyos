use crate::alloc::{GlobalAlloc, Layout, System};
use core::arch::asm;

const SYS_ALLOC: u64 = 2;
const SYS_FREE: u64 = 3;
const SYS_REALLOC: u64 = 4;

unsafe fn syscall_alloc(size: usize, align: usize) -> *mut u8 {
    let ret: u64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") SYS_ALLOC => ret,
            in("rdi") size,
            in("rsi") align,
            out("rcx") _,
            out("r11") _,
        );
    }
    core::ptr::with_exposed_provenance_mut(ret as usize)
}

unsafe fn syscall_free(ptr: *mut u8, size: usize, align: usize) {
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") SYS_FREE => _,
            in("rdi") ptr,
            in("rsi") size,
            in("rdx") align,
            out("rcx") _,
            out("r11") _,
        );
    }
}

unsafe fn syscall_realloc(ptr: *mut u8, size: usize, align: usize, new_size: usize) -> *mut u8 {
    let ret: u64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") SYS_REALLOC => ret,
            in("rdi") ptr,
            in("rsi") size,
            in("rdx") align,
            in("r10") new_size,
            out("rcx") _,
            out("r11") _,
        );
    }
    core::ptr::with_exposed_provenance_mut(ret as usize)
}

#[stable(feature = "alloc_system_type", since = "1.28.0")]
unsafe impl GlobalAlloc for System {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe { syscall_alloc(layout.size(), layout.align()) }
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { syscall_free(ptr, layout.size(), layout.align()) }
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        unsafe { syscall_realloc(ptr, layout.size(), layout.align(), new_size) }
    }
}
