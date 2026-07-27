use core::arch::naked_asm;

/// TLB flush IPI handler. See xhci_entry for register-save rationale.
#[unsafe(naked)]
pub(super) extern "sysv64" fn tlb_flush_entry() {
    naked_asm!(
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
        "lock add dword ptr gs:[240], 1",
        "mov rbp, rsp",
        "and rsp, -16",
        "call {flush}",
        "mov rsp, rbp",
        "mov ecx, 0x80B",
        "xor eax, eax",
        "xor edx, edx",
        "wrmsr",
        "lock sub dword ptr gs:[240], 1",
        "test dword ptr [rsp + 88], 3",
        "jz 1f",
        "cli",
        "mov rbp, rsp",
        "and rsp, -16",
        "call {exit_to_user}",
        "mov rsp, rbp",
        "1:",
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
        exit_to_user = sym crate::arch::idt::kernel_exit_to_user_check,
    );
}

fn flush() {
    crate::mm::paging::flush_tlb_all();
}
