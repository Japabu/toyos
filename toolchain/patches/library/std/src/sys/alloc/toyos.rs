use crate::alloc::{GlobalAlloc, Layout, System};

unsafe extern "C" {
    fn toyos_alloc(size: usize, align: usize) -> *mut u8;
    fn toyos_free(ptr: *mut u8, size: usize, align: usize);
    fn toyos_realloc(ptr: *mut u8, size: usize, align: usize, new_size: usize) -> *mut u8;
}

#[stable(feature = "alloc_system_type", since = "1.28.0")]
unsafe impl GlobalAlloc for System {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe { toyos_alloc(layout.size(), layout.align()) }
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { toyos_free(ptr, layout.size(), layout.align()) }
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        unsafe { toyos_realloc(ptr, layout.size(), layout.align(), new_size) }
    }
}
