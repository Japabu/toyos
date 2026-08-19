//! The MSI and MSI-X capability structures, decoded as pure functions.
//!
//! A message-signalled interrupt is a DMA write the *device* performs, to an
//! address the kernel programs. Everything that decides that address comes out
//! of registers the device published: which BAR its table lives in, how far
//! into it, how wide its address register is. So every one of those numbers is
//! untrusted, and untrusted here means refused rather than corrected — a
//! function that names a reserved BAR indicator is not a function to truncate
//! into range, it is a function whose interrupts this kernel declines to arm.
//!
//! No I/O and no register writes: the effects belong to `drivers/pci.rs`,
//! which is the one place in the kernel that touches either capability.
//!
//! `no_std`, no allocation, no `unsafe`.

#![no_std]
#![forbid(unsafe_code)]

pub mod msi;
pub mod msix;
