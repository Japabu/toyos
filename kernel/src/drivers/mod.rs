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

use alloc::vec::Vec;
use crate::mm::pmm::PhysPage;
use crate::mm::KernelSlice;

/// Contiguous DMA memory backed by 2MB physical pages from the PMM.
/// Returns a KernelSlice for bounds-checked CPU access.
/// Physical address for device descriptors via `slice.phys()`.
pub struct DmaPool {
    _pages: Vec<PhysPage>,
    slice: KernelSlice,
}

unsafe impl Send for DmaPool {}

impl DmaPool {
    pub fn alloc(size: usize) -> Self {
        let pages_2m = (size + crate::mm::PAGE_2M as usize - 1) / crate::mm::PAGE_2M as usize;
        let pages = crate::mm::pmm::alloc_contiguous(pages_2m)
            .expect("DmaPool: out of physical memory");
        let base = pages[0].direct_map().as_mut_ptr::<u8>();
        let slice = unsafe { KernelSlice::from_raw(base, pages_2m * crate::mm::PAGE_2M as usize) };
        Self { _pages: pages, slice }
    }

    pub fn slice(&self) -> KernelSlice {
        self.slice
    }
}
