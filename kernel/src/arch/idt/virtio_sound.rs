use super::msix::msix_entry;
use crate::irq_ring::IrqSource;

/// Rust half of the MSI-X handler. Lock-free and heap-free: it may interrupt
/// a CPU that holds the AUDIO lock (the lock disables preemption, not IRQs).
extern "sysv64" fn virtio_sound_handler() {
    // Timestamp first — this is the hardware-completion time the DLL feeds on.
    let timestamp = crate::clock::nanos_since_boot();
    let mask = crate::drivers::virtio_sound::isr_drain_tx();
    if mask != 0 {
        crate::audio::isr_push_completion(mask, timestamp);
        crate::irq_ring::isr_publish(IrqSource::Audio, timestamp);
        // Force a scheduler entry on IRQ return so drain_irqs converts the
        // record into wakes now, not at the next 10ms quantum tick.
        crate::preempt::set_need_resched();
    }
    crate::arch::apic::eoi();
}

msix_entry! {
    /// Virtio-sound MSI-X entry (see `msix_entry` for the asm contract).
    pub(super) fn virtio_sound_entry => virtio_sound_handler
}
