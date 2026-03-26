use core::arch::naked_asm;
use core::sync::atomic::{AtomicU64, Ordering};

static CPU_BUSY_TICKS: AtomicU64 = AtomicU64::new(0);
static CPU_TOTAL_TICKS: AtomicU64 = AtomicU64::new(0);

pub fn cpu_ticks() -> (u64, u64) {
    (CPU_BUSY_TICKS.load(Ordering::Relaxed), CPU_TOTAL_TICKS.load(Ordering::Relaxed))
}

// Ring 3: save all registers (GPR + SSE/XMM), call Rust handler (preempt), restore, iretq.
// Ring 0: just EOI and return (no preemption while kernel code runs).
#[unsafe(naked)]
pub(super) extern "sysv64" fn timer_entry() {
    naked_asm!(
        // No error code for interrupts. CS is at [rsp + 8].
        "test dword ptr [rsp + 8], 3",
        "jz 2f",

        // Ring 3: preempt — save GPRs
        "push 0", // dummy error code for stack layout consistency
        "push r15", "push r14", "push r13", "push r12",
        "push r11", "push r10", "push r9",  "push r8",
        "push rbp", "push rdi", "push rsi", "push rdx",
        "push rcx", "push rbx", "push rax",

        // Save SSE state (XMM0-XMM15 + MXCSR) — must happen before any Rust
        // code runs, since XMM registers are caller-saved in the System V ABI.
        "sub rsp, 8",           // MXCSR (4 bytes, padded to 8 for alignment)
        "stmxcsr [rsp]",
        "sub rsp, 256",         // 16 × 16 bytes for XMM0-XMM15
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

        "call {handler}",

        // Restore SSE state
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

        // Restore GPRs
        "pop rax",  "pop rbx",  "pop rcx",  "pop rdx",
        "pop rsi",  "pop rdi",  "pop rbp",
        "pop r8",   "pop r9",   "pop r10",  "pop r11",
        "pop r12",  "pop r13",  "pop r14",  "pop r15",
        "add rsp, 8", // pop dummy error code
        "iretq",

        // Ring 0: just EOI. One-shot timer fired, nothing to do in kernel context.
        "2:",
        "push rax",
        "push rdx",
        "mov rdx, [rip + LAPIC_BASE]",
        "mov dword ptr [rdx + 0xB0], 0",
        "pop rdx",
        "pop rax",
        "iretq",
        handler = sym timer_handler,
    );
}

extern "sysv64" fn timer_handler() {
    crate::arch::apic::eoi();
    CPU_BUSY_TICKS.fetch_add(1, Ordering::Relaxed);
    CPU_TOTAL_TICKS.fetch_add(1, Ordering::Relaxed);

    // Process xHCI events (keyboard/mouse) from preemption context,
    // in case the idle loop isn't running on CPU 0.
    if crate::arch::percpu::cpu_id() == 0 {
        crate::drivers::xhci::poll_if_pending();
    }

    crate::scheduler::preempt();
}
