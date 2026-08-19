//! The vector `log-nested-emit` sends itself, and nothing else ever raises.
//!
//! **It exists only in the test kernel.** The gate is `#[cfg]`-installed after
//! `install_gates`, so a shipping kernel has no entry at
//! [`LOG_NEST_VECTOR`](super::LOG_NEST_VECTOR) at all and this file is not
//! compiled — an unraised vector in a shipping IDT would be exactly the dead
//! code the tree's own rule deletes.
//!
//! It borrows the device entry's asm shape for the reason that macro exists:
//! this handler is delivered to **Ring 0** — inside `emit`, which is where the
//! whole point of it is — and that entry is the one in the tree that saves the
//! scratch registers and aligns the stack for an entry from either ring.

use super::device_irq::device_irq_entry;

/// Rust half: one shard generation of patterned records, or nothing at all if
/// this delivery was not the one the injection owed.
extern "sysv64" fn log_nest_handler() {
    crate::log::nested::deliver();
    crate::arch::apic::eoi();
}

device_irq_entry! {
    /// The self-IPI `log::nested` sends from inside `emit`.
    pub(super) fn log_nest_entry => log_nest_handler
}
