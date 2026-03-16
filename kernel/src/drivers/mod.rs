pub mod mmio;
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

/// Page-aligned DMA memory pool for device I/O buffers.
#[repr(C, align(4096))]
pub struct DmaPool<const N: usize> {
    pages: [[u8; 4096]; N],
}

impl<const N: usize> DmaPool<N> {
    pub const fn new() -> Self {
        Self { pages: [[0; 4096]; N] }
    }

    /// Physical address of a DMA page (for device descriptors and registers).
    pub fn page_phys(&self, index: usize) -> u64 {
        self.pages[index].as_ptr() as u64 - crate::PHYS_OFFSET
    }

    /// Virtual pointer to a DMA page (for kernel read/write).
    pub fn page_ptr(&self, index: usize) -> *mut u8 {
        self.pages[index].as_ptr() as *mut u8
    }
}
