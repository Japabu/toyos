//! Device drivers.
//!
//! Every unsafe block under `drivers::` carries a `SAFETY:` comment, and the
//! comment says why the block could not be *removed* as well as why it is
//! sound — the owner's 2026-08-20 ruling, reduction before documentation.
//! `host-tests.yml`'s kernel clippy step already runs with `-D warnings`, so
//! the `warn` below is what actually gates: a new undocumented block anywhere
//! in this module tree fails CI.
//!
//! **What is left is almost all one shape**: a bounds-checked
//! [`crate::mm::KernelSlice`] over memory a [`DmaPool`] allocated, read or
//! written through `unsafe fn`s because `KernelSlice`'s accessors are unsafe.
//! A typed, volatile, `size_of::<T>()`-bounded view handed out by `DmaPool`
//! itself would delete most of them at once, and it is filed rather than built
//! here because it is one abstraction across every driver:
//! `issues/kernel/dma-pool-hands-out-raw-access-not-a-view.md`.
#![warn(clippy::undocumented_unsafe_blocks)]

pub mod serial;
pub mod acpi;
pub mod i8042;
pub mod ioapic;
pub mod pci;
pub mod nvme;
pub mod xhci;
pub mod usb_storage;
pub mod virtio;
pub mod virtio_console;
pub mod virtio_gpu;
pub mod virtio_net;
pub mod virtio_sound;
pub mod gop;
pub mod hda;
pub mod panic_console;

use alloc::vec::Vec;
use crate::mm::pmm::PhysPage;
use crate::mm::KernelSlice;

/// Contiguous DMA memory backed by 2MB physical pages from the PMM.
/// Returns a KernelSlice for bounds-checked CPU access.
/// Physical address for device descriptors via `slice.phys()`.
///
/// **`Send` is derived, not asserted.** `PhysPage` is two integers and
/// `KernelSlice` carries its own `unsafe impl Send`, so the auto trait already
/// holds — the manual `unsafe impl Send for DmaPool {}` that stood here was
/// redundant and is gone. `cargo check` proves it: replacing the impl with a
/// `T: Send` probe over `DmaPool` compiles, where the same probe over
/// `NvmeBlockDevice`, `VConsole`, `GpuController` and `VirtioNic` does not.
pub struct DmaPool {
    _pages: Vec<PhysPage>,
    slice: KernelSlice,
}

impl DmaPool {
    pub fn alloc(size: usize) -> Self {
        let pages_2m = size.div_ceil(crate::mm::PAGE_2M as usize);
        let pages = crate::mm::pmm::alloc_contiguous(pages_2m, crate::mm::pmm::Category::Dma)
            .expect("DmaPool: out of physical memory");
        let base = pages[0].direct_map().as_mut_ptr::<u8>();
        // SAFETY: irreducible — `from_raw` is the only constructor `KernelSlice`
        // has, and this is the one call in the drivers that builds one, so the
        // unsafety cannot be pushed anywhere further down. Its requirement holds
        // here as well as it can be made to: `alloc_contiguous` just returned
        // `pages_2m` physically contiguous 2 MB pages, `pages[0].direct_map()`
        // is their first byte in the direct map, and the pages are moved into
        // `_pages` on the next line so the region outlives every `KernelSlice`
        // copied out of `slice()` — for as long as the pool itself is held,
        // which is the residual `KernelSlice::from_raw` cannot check and
        // `issues/design-debt/kernelslice-from-raw-cannot-check-itself.md`
        // tracks.
        let slice = unsafe { KernelSlice::from_raw(base, pages_2m * crate::mm::PAGE_2M as usize) };
        Self { _pages: pages, slice }
    }

    pub fn slice(&self) -> KernelSlice {
        self.slice
    }
}
