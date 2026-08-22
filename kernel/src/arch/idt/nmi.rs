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
//!
//! # Why this gate has an IST, and what that costs
//!
//! "Between arbitrary instructions" includes the three of `arch::syscall`'s
//! entry that run at CPL 0 with `rsp` still pointing into the user's stack, and
//! the one between its `pop rsp` and its `sysretq`. A frame the CPU builds
//! there is a supervisor write to a user page: SMAP refuses it, the `#PF` lands
//! on the same stack, and the machine takes a `#DF` — measured on this tree with
//! `TF` in that window before the bit joined `IA32_FMASK`. An IST index is the
//! architecture's answer (SDM Vol. 3A §6.14.5): the CPU loads `rsp` from the TSS
//! before it pushes anything, whatever the interrupted context held.
//!
//! **The cost is that an IST stack is not re-entrant.** A second NMI entered
//! while the first is still on IST2 starts its frame at the same top and writes
//! over the first — the corruption Linux's nested-NMI machinery exists to
//! survive (`arch/x86/entry/entry_64.S`: the "NMI executing" variable and the
//! copied `iret` frame, which let its handler take faults and run `int3`
//! breakpoints). This kernel needs none of that, and the reason is a property of
//! *this* handler rather than a claim about NMIs in general:
//!
//! 1. **The architecture blocks NMI delivery from the moment one is delivered
//!    until the next `iretq`** (SDM Vol. 3A §6.7.1). So the only way to re-enter
//!    is an `iretq` executed before this handler is done with its stack.
//! 2. **This handler executes exactly one `iretq`, as its last instruction.**
//! 3. **It cannot fault, so it cannot reach anybody else's `iretq` either.**
//!    What it touches, in full: its own IST2 stack (a boot-time `alloc_zeroed`
//!    in the direct map, never freed); `gs:[…]`, whose base is this CPU's
//!    `PerCpu`, also a direct-map allocation; and, inside
//!    [`crate::sched::dump::note_nmi`], two `static` arrays in `.bss` indexed
//!    after a bounds check that returns early. Every one of those addresses is
//!    covered by a PML4 entry built before the first process existed and
//!    shallow-copied into every address space (`AddressSpace::new_user`), so no
//!    page walk from here can miss, whatever `CR3` holds. It takes no lock, makes
//!    no allocation, logs nothing, dereferences no user pointer and calls
//!    nothing that can panic.
//!
//! That is an argument, and an argument is not an observation — so the entry
//! below *checks* it. `PerCpu::nmi_active` is raised before the first push and
//! cleared before the `iretq`; an NMI that finds it already raised takes
//! [`nested_nmi`], which reports straight to the UART and halts the machine.
//! Silent corruption of the one diagnostic that answers a wedged CPU is worth
//! less than a machine that stops and says so.
//!
//! **Neither path here may `log!`, and the dying one no more than the ordinary
//! one** — the reason is the shard and not the severity, and it is the whole of
//! `src/sourcegate.rs`'s `nmi_does_not_log`.
//!
//! **What gates this, and what does not.** An NMI *arriving inside that window*
//! is staged by `syscall-window-nmi`, and it is reachable on one accelerator
//! only: under TCG, QEMU checks for a pending interrupt between translation
//! blocks and `syscall` ends one, so the dev host delivers 36 to 47 arrivals per
//! 3,000; under KVM the injection happens at a VM entry that is never one of
//! these three instructions — **0 of 6,000 on the hosted lane** (run
//! 32584121311), with thousands of the same NMIs arriving in Ring 3, so the aim
//! was right and the delivery point is simply elsewhere. CI's guest lane is KVM
//! (`tests/CLAUDE.md`), so **"the window is exercised per pull request" is a
//! claim this tree may not make.** What holds on both accelerators is the
//! `nmi-without-ist` control, which takes this gate's IST index off and double
//! faults at `syscall_entry` with `cr2 = rsp - 8`, and the compile-time
//! assertion over `arch::idt`'s table that vector 2 and vector 18 carry an index
//! at all.

use core::arch::naked_asm;

use crate::arch::percpu::OFF_NMI_ACTIVE;

/// Ten pushes of eight bytes, so the interrupt frame's `rip` is here.
const RIP_OFFSET: usize = 80;
/// …and its `cs` and `rsp`, which are what say whether this NMI landed in the
/// window the IST exists for: a Ring 0 frame whose `rsp` is a user address.
const CS_OFFSET: usize = RIP_OFFSET + 8;
const RSP_OFFSET: usize = RIP_OFFSET + 24;

/// Before any push, the CPU's own five words start at `rsp`.
const NESTED_RIP_OFFSET: usize = 0;
const NESTED_RSP_OFFSET: usize = 24;

#[unsafe(naked)]
pub(super) extern "sysv64" fn nmi_entry() {
    naked_asm!(
        // The `cld` `arch::entry::ring3_naked_asm` gives every other gate, at the
        // one entry that is not routed through it. An NMI arrives between
        // arbitrary instructions — `memmove`'s `std` … `cld` window included —
        // and `note` is a `sysv64` call, which the ABI says may not be entered
        // with the direction flag set.
        "cld",
        // The re-entrancy check, before anything is pushed and before any
        // register is touched: `cmp` writes flags, and `iretq` restores the
        // interrupted context's `RFLAGS` from the frame, so flags are free here.
        "cmp dword ptr gs:[{active}], 0",
        "jne 2f",
        "mov dword ptr gs:[{active}], 1",
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
        "mov rsi, [rsp + {cs_offset}]",
        "mov rdx, [rsp + {rsp_offset}]",
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
        // Cleared *before* the `iretq` and never after: NMI delivery is blocked
        // until that instruction retires, so there is no window between the two
        // in which a second NMI could find the word clear.
        "mov dword ptr gs:[{active}], 0",
        // No EOI: an NMI is not delivered through the IRR and acknowledging one
        // would clear an unrelated interrupt's bit.
        "iretq",
        // The loud path. This frame has already overwritten the top of the
        // outer handler's stack, so there is nothing to return to and nothing
        // to preserve — which is why it reads its two words before aligning.
        "2:",
        "mov rdi, [rsp + {nested_rip}]",
        "mov rsi, [rsp + {nested_rsp}]",
        "and rsp, -16",
        "call {nested}",
        "ud2",
        active = const OFF_NMI_ACTIVE,
        rip_offset = const RIP_OFFSET,
        cs_offset = const CS_OFFSET,
        rsp_offset = const RSP_OFFSET,
        nested_rip = const NESTED_RIP_OFFSET,
        nested_rsp = const NESTED_RSP_OFFSET,
        note = sym note,
        nested = sym nested_nmi,
    );
}

/// The entry loads all three words in both builds — two `mov`s on a path only
/// Ctrl+Alt+D reaches — so that the observer and the shipping handler take the
/// same frame. What the shipping kernel does with `cs` and `rsp` is nothing.
extern "sysv64" fn note(rip: u64, cs: u64, rsp: u64) {
    #[cfg(not(feature = "boot-actuators"))]
    let _ = (cs, rsp);
    #[cfg(feature = "boot-actuators")]
    crate::nmi_gate::observe(rip, cs, rsp);
    crate::sched::dump::note_nmi(rip);
    #[cfg(feature = "boot-actuators")]
    crate::nmi_gate::stage_nested_if_armed();
}

/// A second NMI on a stack the first is still standing on.
///
/// **Straight to the UART, and to nothing else — this path may not log either.**
/// The rule the module header states does not relax because the machine is
/// dying: the context this NMI interrupted may be between its own reservation's
/// `xadd` and its body publication, so a record emitted from here would take a
/// newer generation of the same shard slot and garble the very ring
/// `halt_all_cpus` is about to paint. `src/sourcegate.rs`'s `nmi_does_not_log`
/// is the gate, and it caught exactly that on the first draft of this function.
///
/// `panic_raw` is an `outb` loop with no lock in it, so it cannot be blocked by
/// whatever either interrupted context was holding. The cost is that a machine
/// with no 16550 — the T14 as the owner flashes it — halts here with the panel
/// showing whatever the ring already had. That is the honest trade: this path is
/// unreachable in a correct kernel, and the alternative is corrupting the report
/// on every machine to have a sentence on one.
extern "sysv64" fn nested_nmi(rip: u64, rsp: u64) -> ! {
    let serial = crate::drivers::serial::panic_raw;
    serial(b"\n[nmi] NESTED NMI on cpu ");
    crate::drivers::serial::panic_raw_dec(u64::from(crate::arch::percpu::cpu_id()));
    serial(b": a second NMI entered while IST2 was still in use.\n[nmi]   rip=");
    crate::drivers::serial::panic_raw_hex(rip);
    serial(b" rsp=");
    crate::drivers::serial::panic_raw_hex(rsp);
    serial(b"\n[nmi]   the outer handler's frame is gone; the machine stops here.\n");
    crate::arch::apic::halt_all_cpus()
}
