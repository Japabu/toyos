use super::msix::msix_entry;
use crate::irq_ring::IrqSource;

/// Rust half of the MSI-X handler. Lock-free and heap-free — RX descriptors
/// are drained by the record's consumer (drain_events → netd wake), never here.
extern "sysv64" fn virtio_net_handler() {
    let timestamp = crate::clock::nanos_since_boot();
    crate::irq_ring::isr_publish(IrqSource::Net, timestamp);
    // Force a scheduler entry on IRQ return so drain_events converts the
    // record into wakes/CQEs now, not at the next 10ms quantum tick.
    crate::preempt::set_need_resched();
    crate::arch::apic::eoi();
}

msix_entry! {
    /// Virtio-net MSI-X entry (see `msix_entry` for the asm contract).
    pub(super) fn virtio_net_entry => virtio_net_handler
}
