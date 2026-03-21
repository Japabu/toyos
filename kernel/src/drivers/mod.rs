pub mod serial;
pub mod acpi;
pub mod pci;
pub mod nvme;
pub mod xhci;
pub mod virtio;
pub mod virtio_gpu;
pub mod virtio_net;
pub mod virtio_sound;
pub mod gop;

use crate::DmaAddr;

/// DMA memory pool backed by heap-allocated pages at 2MB alignment.
/// Each pool gets its own dedicated 2MB-aligned physical region, ensuring
/// it never shares a 2MB page with kernel .data.
pub struct DmaPool {
    base: *mut u8,
    page_count: usize,
}

unsafe impl Send for DmaPool {}

impl DmaPool {
    pub fn alloc(page_count: usize) -> Self {
        use core::alloc::Layout;
        let size = page_count * 4096;
        let align = crate::mm::PAGE_2M as usize;
        let layout = Layout::from_size_align(size, align).expect("DmaPool: invalid layout");
        let base = unsafe { alloc::alloc::alloc_zeroed(layout) };
        assert!(!base.is_null(), "DmaPool: allocation failed ({} pages)", page_count);
        Self { base, page_count }
    }

    pub fn page_phys(&self, index: usize) -> DmaAddr {
        assert!(index < self.page_count, "DmaPool: index {index} out of range {}", self.page_count);
        let ptr = unsafe { self.base.add(index * 4096) };
        DmaAddr::from_ptr(ptr)
    }

    pub fn page_ptr(&self, index: usize) -> *mut u8 {
        assert!(index < self.page_count, "DmaPool: index {index} out of range {}", self.page_count);
        unsafe { self.base.add(index * 4096) }
    }
}
