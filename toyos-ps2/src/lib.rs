//! PS/2 wire decoding: scancodes and mouse packets in, HID usages and
//! pointer deltas out.
//!
//! Pure `no_std` with no dependencies and no state beyond a byte or two,
//! because that is what makes it testable on the host. The logic is small
//! and its defect density is not: fake shifts, the Pause sequence, 9-bit
//! sign extension, and resync after a lost byte are each one line away from
//! a bug that is invisible in QEMU and un-single-steppable on the laptop
//! this exists for.
//!
//! The kernel side of the driver (`kernel/src/drivers/i8042/`) owns the
//! controller, the interrupt and the queues. Nothing in here touches
//! hardware, allocates, or knows what a lock is.

#![no_std]

pub mod key;
pub mod mouse;

pub use key::{KeyDecoder, KeyOutcome};
pub use mouse::{MouseDecoder, MouseOutcome};
