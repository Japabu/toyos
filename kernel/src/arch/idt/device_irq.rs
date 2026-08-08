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
//! the Ring 3 epilogue may context-switch — it therefore parks the user machine
//! state on this kernel stack across the call (`arch::entry`): other threads'
//! user code clobbers it, while kernel code itself is soft-float and never
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
            $crate::arch::entry::ring3_naked_asm!(
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
                // Ring 3: run the deferred-preempt epilogue with the user
                // machine state parked on this kernel stack across any context
                // switch. The bracket leaves rsp aligned for the call.
                $crate::arch::entry::save_user_state!(),
                "call {exit_to_user}",
                $crate::arch::entry::restore_user_state!(),
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
