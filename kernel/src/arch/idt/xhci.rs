use core::arch::naked_asm;
use core::sync::atomic::{AtomicU32, Ordering};

#[no_mangle]
static XHCI_IRQ_PENDING: AtomicU32 = AtomicU32::new(0);

/// Atomically check and clear the xHCI interrupt pending flag.
pub fn xhci_irq_pending() -> bool {
    XHCI_IRQ_PENDING.swap(0, Ordering::Acquire) != 0
}

/// xHCI MSI-X handler — minimal: set atomic flag + EOI. No Rust call needed.
#[unsafe(naked)]
pub(super) extern "sysv64" fn xhci_entry() {
    naked_asm!(
        "push rax",
        "push rdx",
        "lock bts dword ptr [rip + {flag}], 0",
        "mov rdx, [rip + LAPIC_BASE]",
        "mov dword ptr [rdx + 0xB0], 0",
        "pop rdx",
        "pop rax",
        "iretq",
        flag = sym XHCI_IRQ_PENDING,
    );
}
