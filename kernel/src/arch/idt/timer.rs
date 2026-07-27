use core::arch::naked_asm;

// Ring 0 path re-arms the one-shot timer itself: without re-arming, a fire
// while in Ring 0 would silently disable preemption forever. need_resched
// gets picked up at the next kernel→user exit.
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

        // Re-arm before Rust runs so the timer survives even if the handler
        // path panics before scheduler::do_preempt → arm_one_shot.
        // gs:[260] = PerCpu.last_armed_ticks (per-CPU one-shot re-arm value).
        "mov ecx, 0x838",
        "mov eax, dword ptr gs:[260]",
        "xor edx, edx",
        "wrmsr",

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

        "2:",
        "push rax",
        "push rcx",
        "push rdx",
        "mov ecx, 0x80B",       // X2APIC_EOI
        "xor eax, eax",
        "xor edx, edx",
        "wrmsr",
        "mov ecx, 0x838",       // X2APIC_TIMER_INIT — re-arm with last value;
        "mov eax, dword ptr gs:[260]",  // PerCpu.last_armed_ticks; 0 = disabled.
        "xor edx, edx",
        "wrmsr",
        "mov byte ptr gs:[244], 1",     // need_resched
        "inc dword ptr gs:[248]",       // ring0_timer_fires (no lock: single writer, IF=0)
        "pop rdx",
        "pop rcx",
        "pop rax",
        "iretq",
        handler = sym timer_handler,
    );
}

extern "sysv64" fn timer_handler() {
    crate::trace::trace(crate::trace::TraceKind::TimerFire, 0);
    crate::arch::apic::eoi();

    // Process xHCI events (keyboard/mouse) from preemption context, in case
    // this CPU's IRQ-exit drain was deferred (Ring 0 interrupt during a long
    // preempt-off section). No-op unless this CPU holds the irq_ring record.
    crate::drivers::xhci::poll_if_pending();

    // Drain pending log output. With sustained user-mode load both CPUs
    // never enter the idle loop, so without a tick-driven drain the ring
    // would silently fill and never reach the host. try_lock keeps this
    // non-blocking — if another CPU is already draining we skip. One
    // bounded chunk only: this runs with IF=0 ahead of do_preempt, and an
    // unbounded drain (up to 64KiB of per-byte UART waits) would add
    // milliseconds of preemption latency. The idle loop drains fully.
    if let Some(mut g) = crate::drivers::serial::BackendGuard::try_lock() {
        crate::drivers::log_ring::drain_chunk_to_serial(&mut g);
    }

    crate::scheduler::do_preempt();
}
