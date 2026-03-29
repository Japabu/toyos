use core::arch::naked_asm;

/// TLB flush IPI handler — flush TLB + EOI.
#[unsafe(naked)]
pub(super) extern "sysv64" fn tlb_flush_entry() {
    naked_asm!(
        // Save all caller-saved GPRs + rbp (used for stack alignment)
        "push rax",
        "push rcx",
        "push rdx",
        "push rsi",
        "push rdi",
        "push r8",
        "push r9",
        "push r10",
        "push r11",
        "push rbp",
        // Align stack to 16 bytes for Rust ABI
        "mov rbp, rsp",
        "and rsp, -16",
        "call {flush}",
        "mov rsp, rbp",
        // EOI via x2APIC MSR
        "mov ecx, 0x80B",
        "xor eax, eax",
        "xor edx, edx",
        "wrmsr",
        "pop rbp",
        "pop r11",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rdi",
        "pop rsi",
        "pop rdx",
        "pop rcx",
        "pop rax",
        "iretq",
        flush = sym flush,
    );
}

fn flush() {
    crate::mm::paging::flush_tlb_all();
}
