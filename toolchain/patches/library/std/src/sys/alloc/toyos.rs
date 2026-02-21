use crate::alloc::{GlobalAlloc, Layout, System};

#[stable(feature = "alloc_system_type", since = "1.28.0")]
unsafe impl GlobalAlloc for System {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe { crate::sys::toyos_alloc(layout.size(), layout.align()) }
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { crate::sys::toyos_free(ptr, layout.size(), layout.align()) }
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        unsafe { crate::sys::toyos_realloc(ptr, layout.size(), layout.align(), new_size) }
    }
}
