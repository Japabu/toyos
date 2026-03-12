use core::arch::naked_asm;

/// TLB flush IPI handler — reload CR3 + EOI.
#[unsafe(naked)]
pub(super) extern "sysv64" fn tlb_flush_entry() {
    naked_asm!(
        "push rax",
        "push rdx",
        "mov rax, cr3",
        "mov cr3, rax",
        "mov rdx, [rip + LAPIC_BASE]",
        "mov dword ptr [rdx + 0xB0], 0",
        "pop rdx",
        "pop rax",
        "iretq",
    );
}
