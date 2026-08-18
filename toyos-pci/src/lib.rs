//! A PCI function's Base Address Registers and its two interrupt capability
//! structures, decoded as pure functions.
//!
//! A message-signalled interrupt is a DMA write the *device* performs, to an
//! address the kernel programs. Everything that decides that address comes out
//! of registers the device published: which BAR its table lives in, how far
//! into it, how wide its address register is. So every one of those numbers is
//! untrusted in the sense `specs/device-test-strategy.md` means — a function
//! that names a reserved BAR indicator is not a function to truncate into
//! range, it is a function whose interrupts this kernel declines to arm.
//!
//! [`bar`] is that sentence one register lower down, and it is where the
//! reserved-indicator refusal stopped short: `msix` refuses a BAR *index* it
//! cannot use, and then the BAR that index named was decoded by a function
//! with no encoding of an I/O BAR it refused.
//!
//! No I/O and no register writes: the effects belong to `drivers/pci.rs`,
//! which is the one place in the kernel that touches any of the three.
//!
//! `no_std`, no allocation, no `unsafe`.

#![no_std]
#![forbid(unsafe_code)]

pub mod bar;
pub mod msi;
pub mod msix;
