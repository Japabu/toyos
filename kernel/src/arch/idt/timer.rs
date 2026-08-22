use toyos_sched::hw::{CpuId, Machine, TraceEvent, TraceKind};

use crate::arch::entry::{restore_user_state, ring3_naked_asm, save_user_state};
use crate::hw::HW;

// Ring 0 path re-arms the one-shot timer itself: without re-arming, a fire
// while in Ring 0 would silently disable preemption forever. need_resched
// gets picked up at the next kernel→user exit.
//
// **The Ring 3 path runs `kernel_exit_to_user_check` like every other return
// to userland, and it was the one vector that did not.** `apic::kick_cpu` sends
// TIMER_VECTOR, so this stub is where a retire's own IPI lands — and a thread
// killed while running in Ring 3 was preempted here, put in the dying list,
// picked straight back off it with a fresh quantum, and returned to userland
// with the kill pending and nothing on the path that reads it. Once per tick,
// for as long as the thread cared to loop: "a killed thread is never dispatched
// into *userland* again" was false without a bound. `exit_if_killed` lives in
// that epilogue, which is why the fix is to join it rather than to add a second
// check here.
#[unsafe(naked)]
pub(super) extern "sysv64" fn timer_entry() {
    ring3_naked_asm!(
        // No error code for interrupts. CS is at [rsp + 8].
        "test dword ptr [rsp + 8], 3",
        "jz 2f",

        // Ring 3: preempt — save GPRs
        "push 0", // dummy error code for stack layout consistency
        "push r15", "push r14", "push r13", "push r12",
        "push r11", "push r10", "push r9",  "push r8",
        "push rbp", "push rdi", "push rsi", "push rdx",
        "push rcx", "push rbx", "push rax",

        save_user_state!(),

        // Re-arm before Rust runs so the timer survives even if the handler
        // path panics before scheduler::do_preempt → arm_one_shot.
        // gs:[260] = PerCpu.last_armed_ticks (per-CPU one-shot re-arm value).
        "mov ecx, 0x838",
        "mov eax, dword ptr gs:[260]",
        "xor edx, edx",
        "wrmsr",

        "call {handler}",

        // IF is 0 here (interrupt gate, and the handler never sti's), which is
        // the epilogue's entry contract. The user machine state is parked on
        // this kernel stack across it, exactly as `device_irq` parks it.
        "call {exit_to_user}",

        restore_user_state!(),

        // Restore GPRs
        "pop rax",  "pop rbx",  "pop rcx",  "pop rdx",
        "pop rsi",  "pop rdi",  "pop rbp",
        "pop r8",   "pop r9",   "pop r10",  "pop r11",
        "pop r12",  "pop r13",  "pop r14",  "pop r15",
        "add rsp, 8", // pop dummy error code
        "iretq",

        "2:",
        // The census's two `add`s, written here because this branch has no
        // Rust half to put them in — `timer_handler` carries the Ring 3 path's
        // pair. No register and no `lock` (`irq_census`), and the flags they
        // write are dead: the `test` above has already branched.
        "add qword ptr gs:[{irq_total}], 1",
        "add qword ptr gs:[{irq_timer}], 1",
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
        exit_to_user = sym crate::arch::idt::kernel_exit_to_user_check,
        irq_total = const crate::irq_census::slot_offset(crate::irq_census::TOTAL),
        irq_timer = const crate::irq_census::slot_offset(
            1 + crate::irq_census::Source::Timer as usize
        ),
    );
}

extern "sysv64" fn timer_handler() {
    crate::irq_census::irq_took!(Timer);
    // Only the Ring 3 tick reaches here — the stub above branches away first
    // — so the interrupted context is user code and this CPU holds no `Lock`.
    // Everything below rests on that: the pass at the bottom drains the input
    // drivers. `Lock::lock` raises
    // the preempt count, so a nonzero count here is that gate having gone.
    assert_eq!(
        crate::preempt::count(),
        0,
        "the timer handler ran in kernel context, where a lock may be held",
    );

    // Through the `Machine` boundary rather than the ring directly: this
    // handler is the driver entry the cutover builds on, and routing it now
    // is what puts the boundary's trace path on the highest-rate event the
    // kernel has.
    HW.trace(TraceEvent {
        ts: HW.now(),
        cpu: CpuId(crate::arch::percpu::cpu_id()),
        kind: TraceKind::TimerFire,
    });
    crate::arch::apic::eoi();

    crate::scheduler::do_preempt();
}
