use super::device_irq::device_irq_entry;

/// Rust half of the HDA stream-completion handler. Lock-free and heap-free: it
/// may interrupt a CPU that holds the controller lock, which disables
/// preemption and not interrupts.
extern "sysv64" fn hda_handler() {
    crate::irq_census::irq_took!(Hda);
    crate::drivers::hda::isr_complete();
    crate::arch::apic::eoi();
}

device_irq_entry! {
    /// HDA stream MSI entry (see `device_irq_entry` for the asm contract).
    pub(super) fn hda_entry => hda_handler
}
