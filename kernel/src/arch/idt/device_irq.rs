//! Shared device interrupt entry.
//!
//! Every device vector (xHCI, virtio-net, virtio-sound over MSI-X; the i8042
//! on an I/O APIC pin) has the same obligations, so one asm shape serves all
//! of them: save the SysV scratch GPRs + rbp (the Rust handler can clobber
//! them — leaving any unsaved would leak kernel state into user regs on
//! iretq), bracket the handler with the percpu preempt count, and on return
//! to Ring 3 run the deferred-preempt epilogue. How the vector was delivered
//! makes no difference to any of that.
//!
//! Every handler publishes an `irq_ring` record and sets `need_resched`, so
//! the Ring 3 epilogue may context-switch — it therefore parks the user's
//! XMM0-15 + MXCSR on this kernel stack across the call: other threads' user
//! code clobbers XMM, while kernel code itself is soft-float and never
//! touches it (hence no save around the handler call itself).
//!
//! IF stays 0 for the entire entry (interrupt gate; handlers never sti), so
//! `kernel_exit_to_user_check`'s IF=0-on-entry contract holds without an
//! explicit cli.

/// Define a naked device-interrupt entry point that calls `$handler` and runs
/// the deferred-preempt epilogue on the Ring 3 return path.
macro_rules! device_irq_entry {
    ($(#[$meta:meta])* $vis:vis fn $name:ident => $handler:path) => {
        $(#[$meta])*
        #[unsafe(naked)]
        $vis extern "sysv64" fn $name() {
            core::arch::naked_asm!(
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
                // Ring 0 entry has unknown rsp alignment; align via the rbp save.
                "mov rbp, rsp",
                "and rsp, -16",
                "call {handler}",
                "mov rsp, rbp",
                "lock sub dword ptr gs:[240], 1",
                "test dword ptr [rsp + 88], 3", // CS = 10 GPRs + RIP above
                "jz 1f",
                // Ring 3: run the deferred-preempt epilogue with user XMM state
                // parked on this kernel stack across any context switch.
                "sub rsp, 8",
                "stmxcsr [rsp]",
                "sub rsp, 256",
                "movdqu [rsp + 0*16], xmm0",
                "movdqu [rsp + 1*16], xmm1",
                "movdqu [rsp + 2*16], xmm2",
                "movdqu [rsp + 3*16], xmm3",
                "movdqu [rsp + 4*16], xmm4",
                "movdqu [rsp + 5*16], xmm5",
                "movdqu [rsp + 6*16], xmm6",
                "movdqu [rsp + 7*16], xmm7",
                "movdqu [rsp + 8*16], xmm8",
                "movdqu [rsp + 9*16], xmm9",
                "movdqu [rsp + 10*16], xmm10",
                "movdqu [rsp + 11*16], xmm11",
                "movdqu [rsp + 12*16], xmm12",
                "movdqu [rsp + 13*16], xmm13",
                "movdqu [rsp + 14*16], xmm14",
                "movdqu [rsp + 15*16], xmm15",
                // Ring 3 entry: RSP0 is 16-aligned, 5 iretq slots + 10 pushes + 264
                // leave rsp 16-aligned here — call directly, no realign needed.
                "call {exit_to_user}",
                "movdqu xmm0,  [rsp + 0*16]",
                "movdqu xmm1,  [rsp + 1*16]",
                "movdqu xmm2,  [rsp + 2*16]",
                "movdqu xmm3,  [rsp + 3*16]",
                "movdqu xmm4,  [rsp + 4*16]",
                "movdqu xmm5,  [rsp + 5*16]",
                "movdqu xmm6,  [rsp + 6*16]",
                "movdqu xmm7,  [rsp + 7*16]",
                "movdqu xmm8,  [rsp + 8*16]",
                "movdqu xmm9,  [rsp + 9*16]",
                "movdqu xmm10, [rsp + 10*16]",
                "movdqu xmm11, [rsp + 11*16]",
                "movdqu xmm12, [rsp + 12*16]",
                "movdqu xmm13, [rsp + 13*16]",
                "movdqu xmm14, [rsp + 14*16]",
                "movdqu xmm15, [rsp + 15*16]",
                "add rsp, 256",
                "ldmxcsr [rsp]",
                "add rsp, 8",
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
                handler = sym $handler,
                exit_to_user = sym crate::arch::idt::kernel_exit_to_user_check,
            );
        }
    };
}

pub(crate) use device_irq_entry;
