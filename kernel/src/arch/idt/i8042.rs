use super::device_irq::device_irq_entry;

device_irq_entry! {
    /// i8042 pin-interrupt entry (see `device_irq_entry` for the asm
    /// contract). Both PS/2 lines land here; the Rust half is the only
    /// reader of port 0x60 in the kernel and states why.
    pub(super) fn i8042_entry => crate::drivers::i8042::handler
}
