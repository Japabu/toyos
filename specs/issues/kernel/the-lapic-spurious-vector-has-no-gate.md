---
status: open
kind: defect
opened: 2026-08-18
---

# The kernel programs the LAPIC to deliver vector `0xFF` and installs no gate for it

`kernel/src/arch/apic.rs`, `enable_x2apic`, on the BSP and on every AP:

```rust
let svr = cpu::rdmsr(X2APIC_SVR);
cpu::wrmsr(X2APIC_SVR, svr | (1 << 8) | 0xFF);
```

so this machine's spurious-interrupt vector is `0xFF`. `kernel/src/arch/idt/mod.rs`
declares gates for the exception vectors Intel names, for NMI, for `0x20..=0x26`,
for `0xFD` (halt IPI) and for `0xFE` (TLB flush), plus `0x27` in a kernel built
with `boot-actuators`. Every other entry of the 256 is `IdtEntry::EMPTY`, whose
`type_attr` is `0` — **P=0**.

**A fault or interrupt delivered through a gate with P=0 is a contributory fault
and the CPU escalates to `#DF`, which `double_fault_handler` answers with
`halt_all_cpus`.** That is not a deduction from the manual: this tree reproduced
it and fixed it for the exception range in `9bd7a9e` — *"`div` by zero in any
Ring 3 process took the whole guest down … the CPU escalated to #DF … Reproduced
before the fix: `DOUBLE FAULT on CPU 0 (pid=Some(Pid(3)))` and a timed-out test,
wide and alone."* The comment that commit left above `idt_vectors!` states the
rule the SVR write breaks:

> Every vector Intel names for 64-bit mode has a gate, because a vector without
> one does not fault the process: the CPU takes the missing gate as a second,
> contributory fault and escalates to #DF, which halts the machine.

The spurious vector is not one Intel names as an exception; it is one **this
kernel names**, by writing it into SVR. So it is the same rule and the same
mechanism, on the one vector the fix did not reach.

## What is *not* claimed

That this caused
`specs/issues/kernel/a-double-fault-on-cpu-1-under-a-wide-suite.md`. It cannot
be claimed: a spurious interrupt leaves nothing behind, and that sighting's
report was never printed. This file exists because the hole is provable from the
source with no appeal to that sighting at all.

Nor is the delivery rate known, and what can be read of it here is narrow:

- The task-priority register is never written — `X2APIC_TPR` (`0x808`) appears
  nowhere in the tree — so the SDM's classic spurious condition, an interrupt
  masked by a TPR raised between assertion and `INTA`, cannot arise from this
  kernel's own doing.
- `stop_timer` writes `0` to `X2APIC_TIMER_INIT` and does **not** mask the LVT,
  so an already-latched timer interrupt is still delivered on `0x20`.
- The only LVT mask is `wrmsr(X2APIC_LVT_TIMER, 1 << 16)` during BSP
  calibration.
- Every device on this machine is MSI or MSI-X, and the two ISA lines are
  edge-triggered.

So the honest statement is: **the gate is missing and its absence is lethal;
what would deliver it here is not established.** On the T14 — different
firmware, different interrupt topology, a real IOAPIC with level-triggered
lines — that last clause is worth a great deal less.

## What a fix has to get right

Not simply an `iretq`. A genuine spurious interrupt is **not** acknowledged —
the LAPIC set no ISR bit, and an EOI from the handler would clear an unrelated
interrupt's bit instead. But the same vector reached by a self-IPI or by an ICR
send *does* go through the IRR and *does* need one. The handler therefore has to
read the ISR (x2APIC MSRs `0x810..=0x817`) and EOI only when the vector is
actually in service — which is also what makes the fix stageable: `apic::send_self`
already exists (`log/nested.rs` uses it), so a test can raise the vector on
purpose and assert the machine survives, the count moved, and no interrupt after
it was lost.

The count is the second half. A spurious interrupt that is absorbed silently is
a machine hiding an interrupt-routing defect, and the handler may not `log!` —
it can arrive inside the log commit path, which is the whole reason
`LOG_NEST_VECTOR` exists. `kernel/src/arch/idt/nmi.rs` is the shape: a lock-free
per-CPU slot written by the handler and read from ordinary context.

While somebody is there: **236 of the 256 IDT entries are `EMPTY`**, so every
one of them turns an interrupt nobody expected into a halted machine with no
name on it. `9bd7a9e` closed the range Intel defines; the range the platform
defines is still open, and a stale MSI-X entry left by a driver reconfiguration
lands in it.
