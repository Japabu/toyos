//! Device drivers.
//!
//! Every unsafe block under `drivers::` carries a `SAFETY:` comment, and the
//! comment says why the block could not be *removed* as well as why it is
//! sound — the owner's 2026-08-20 ruling, reduction before documentation.
//! `host-tests.yml`'s kernel clippy step already runs with `-D warnings`, so
//! the `warn` below is what actually gates: a new undocumented block anywhere
//! in this module tree fails CI.
//!
//! **What is left is not the DMA shape.** Until 2026-08-22 most of it was: a
//! bounds-checked `KernelSlice` over memory a [`DmaPool`] allocated, read or
//! written through `unsafe fn`s at 35 sites in ten files, each arguing the same
//! three sentences by hand. That is [`crate::mm::Dma`] now — a safe, typed,
//! pool-borrowing view in two disciplines, whose module header says which
//! regions take which and why. **No driver under here may touch DMA memory any
//! other way**: `Dma`'s constructor is private to `mm::dma`, so there is no
//! second door to build one through.
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

/// The pool every driver here allocates its DMA out of. It and the [`crate::mm::Dma`]
/// view it hands out live in `mm::dma`, which is what makes the view's
/// constructor private: a driver can hold a `Dma` and cannot mint one.
pub use crate::mm::DmaPool;
