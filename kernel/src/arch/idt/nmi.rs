//! Vector 2, and the only thing it is for: asking a CPU that will not answer a
//! kick where it is.
//!
//! An NMI is not maskable by `IF`, so it reaches a CPU spinning in a
//! cli-guarded loop — which a kick does not, and which is exactly the state
//! `sched::dump` cannot otherwise tell apart from a halted CPU whose IPI was
//! lost.
//!
//! **The handler does not log.** It cannot: the context it interrupts may hold
//! the log ring's lock, and an NMI that waits on a lock its own victim owns is
//! the deadlock this facility exists to diagnose. It stores `rip` in a
//! lock-free per-CPU slot and returns; the CPU that asked prints and symbolizes
//! it from ordinary context.
//!
//! No preempt-count bump and no exit-to-user check either. An NMI arrives
//! between arbitrary instructions, including inside the window where either of
//! those is half-updated, and this handler never reschedules — so the only
//! correct thing it can do to that state is nothing.

use core::arch::naked_asm;

/// Ten pushes of eight bytes, so the interrupt frame's `rip` is here.
const RIP_OFFSET: usize = 80;

#[unsafe(naked)]
pub(super) extern "sysv64" fn nmi_entry() {
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
        "mov rdi, [rsp + {rip_offset}]",
        "mov rbp, rsp",
        "and rsp, -16",
        "call {note}",
        "mov rsp, rbp",
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
        // No EOI: an NMI is not delivered through the IRR and acknowledging one
        // would clear an unrelated interrupt's bit.
        "iretq",
        rip_offset = const RIP_OFFSET,
        note = sym note,
    );
}

extern "sysv64" fn note(rip: u64) {
    crate::sched::dump::note_nmi(rip);
}
