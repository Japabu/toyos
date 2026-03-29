use core::arch::naked_asm;

/// TLB flush IPI handler — reload CR3 + EOI via x2APIC MSR.
#[unsafe(naked)]
pub(super) extern "sysv64" fn tlb_flush_entry() {
    naked_asm!(
        "push rax",
        "push rcx",
        "push rdx",
        "mov rax, cr3",
        "mov cr3, rax",
        "mov ecx, 0x80B", // X2APIC_EOI
        "xor eax, eax",
        "xor edx, edx",
        "wrmsr",
        "pop rdx",
        "pop rcx",
        "pop rax",
        "iretq",
    );
}
