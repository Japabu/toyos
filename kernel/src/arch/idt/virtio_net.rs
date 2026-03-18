use core::arch::naked_asm;
use core::sync::atomic::{AtomicU32, Ordering};

#[no_mangle]
static VIRTIO_NET_IRQ_PENDING: AtomicU32 = AtomicU32::new(0);

/// Atomically check and clear the virtio-net interrupt pending flag.
pub fn irq_pending() -> bool {
    VIRTIO_NET_IRQ_PENDING.swap(0, Ordering::Acquire) != 0
}

/// Virtio-net MSI-X handler — set atomic flag + EOI.
#[unsafe(naked)]
pub(super) extern "sysv64" fn virtio_net_entry() {
    naked_asm!(
        "push rax",
        "push rdx",
        "lock bts dword ptr [rip + {flag}], 0",
        "mov rdx, [rip + LAPIC_BASE]",
        "mov dword ptr [rdx + 0xB0], 0",
        "pop rdx",
        "pop rax",
        "iretq",
        flag = sym VIRTIO_NET_IRQ_PENDING,
    );
}
