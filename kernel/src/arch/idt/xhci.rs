use core::arch::naked_asm;
use core::sync::atomic::{AtomicU32, Ordering};

#[no_mangle]
static XHCI_IRQ_PENDING: AtomicU32 = AtomicU32::new(0);

/// Atomically check and clear the xHCI interrupt pending flag.
pub fn xhci_irq_pending() -> bool {
    XHCI_IRQ_PENDING.swap(0, Ordering::Acquire) != 0
}

/// xHCI MSI-X handler — minimal: set atomic flag + EOI via x2APIC MSR.
#[unsafe(naked)]
pub(super) extern "sysv64" fn xhci_entry() {
    naked_asm!(
        "push rax",
        "push rcx",
        "push rdx",
        "lock bts dword ptr [rip + {flag}], 0",
        "mov ecx, 0x80B", // X2APIC_EOI
        "xor eax, eax",
        "xor edx, edx",
        "wrmsr",
        "pop rdx",
        "pop rcx",
        "pop rax",
        "iretq",
        flag = sym XHCI_IRQ_PENDING,
    );
}
