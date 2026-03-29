use core::arch::naked_asm;
use core::sync::atomic::{AtomicU32, Ordering};

#[no_mangle]
static VIRTIO_NET_IRQ_PENDING: AtomicU32 = AtomicU32::new(0);

/// Atomically check and clear the virtio-net interrupt pending flag.
pub fn irq_pending() -> bool {
    VIRTIO_NET_IRQ_PENDING.swap(0, Ordering::Acquire) != 0
}

/// Virtio-net MSI-X handler — set atomic flag + EOI via x2APIC MSR.
#[unsafe(naked)]
pub(super) extern "sysv64" fn virtio_net_entry() {
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
        flag = sym VIRTIO_NET_IRQ_PENDING,
    );
}
