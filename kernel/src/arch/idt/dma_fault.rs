use super::device_irq::device_irq_entry;

/// The remapping unit's fault event.
///
/// The same entry shape as a device vector because the delivery is the same —
/// an MSI to the boot CPU — but not the same kind of event: this fires when
/// the unit has *blocked* a device, so what it reports is a bug in whoever
/// owns that device rather than work the device has finished. It publishes no
/// `irq_ring` record and sets no `need_resched`; at this stage every stream on
/// the machine is kernel-owned, so the handler's own verdict is to stop.
extern "sysv64" fn dma_fault_handler() {
    crate::irq_census::irq_took!(DmaFault);
    crate::iommu::fault_interrupt();
}

device_irq_entry! {
    /// DMA remapping fault entry (see `device_irq_entry` for the asm contract).
    pub(super) fn dma_fault_entry => dma_fault_handler
}
