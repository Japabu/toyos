use super::device_irq::device_irq_entry;
use crate::irq_ring::IrqSource;

/// Rust half of the MSI-X handler. Lock-free and heap-free — the event ring
/// is polled by the record's consumer (`drivers::xhci::poll_if_pending`),
/// never here.
extern "sysv64" fn xhci_handler() {
    crate::irq_census::irq_took!(Xhci);
    let timestamp = crate::clock::nanos_since_boot();
    crate::irq_ring::isr_publish(IrqSource::Xhci, timestamp);
    // Force a scheduler entry on IRQ return so drain_irqs polls the
    // controller now, not at the next 10ms quantum tick.
    crate::preempt::set_need_resched();
    crate::arch::apic::eoi();
}

device_irq_entry! {
    /// xHCI MSI-X entry (see `device_irq_entry` for the asm contract).
    pub(super) fn xhci_entry => xhci_handler
}
