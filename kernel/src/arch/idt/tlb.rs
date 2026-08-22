use crate::arch::entry::{restore_user_state, ring3_naked_asm, save_user_state};

/// TLB flush IPI handler.
///
/// The GPR save is `device_irq_entry`'s, for its reasons: the Rust half may
/// clobber every System V scratch register, and leaving one unsaved leaks
/// kernel state into a user register on `iretq`. The user machine state is
/// parked for the Ring 3 epilogue alone, which is the only window here that can
/// context-switch — a comment used to point at `xhci_entry` for the rationale
/// and then not follow it, which is how this vector spent its life returning to
/// userland with another thread's XMM registers.
#[unsafe(naked)]
pub(super) extern "sysv64" fn tlb_flush_entry() {
    ring3_naked_asm!(
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
        save_user_state!(),
        "call {exit_to_user}",
        restore_user_state!(),
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
    crate::irq_census::irq_took!(Tlb);
    crate::arch::tlb::serve_ipi();
}
