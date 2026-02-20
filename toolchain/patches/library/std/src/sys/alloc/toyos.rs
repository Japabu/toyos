use crate::alloc::{GlobalAlloc, Layout, System};
use crate::sys::syscall;

const SYS_ALLOC: u64 = 2;
const SYS_FREE: u64 = 3;
const SYS_REALLOC: u64 = 4;

#[stable(feature = "alloc_system_type", since = "1.28.0")]
unsafe impl GlobalAlloc for System {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ret = syscall(SYS_ALLOC, layout.size() as u64, layout.align() as u64, 0, 0);
        core::ptr::with_exposed_provenance_mut(ret as usize)
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        syscall(SYS_FREE, ptr as u64, layout.size() as u64, layout.align() as u64, 0);
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let ret = syscall(SYS_REALLOC, ptr as u64, layout.size() as u64, layout.align() as u64, new_size as u64);
        core::ptr::with_exposed_provenance_mut(ret as usize)
    }
}
