---
status: open
kind: defect
opened: 2026-08-08
---

# `X86_BUG_SYSRET_SS_ATTRS` is unfixed and unmeasured

Filed out of the AMD `#GP` entry when that closed; the RPL fix does not reach this.

AMD's `SYSRET` does not reload SS's cached descriptor, so a `sysretq` taken
while `SS` is NULL leaves userland with an `SS` that *looks* like `0x1b` and
faults when used. **The kernel can reach it**: an IDT entry from Ring 3 nulls
SS, `timer_handler` calls `do_preempt`, and the incoming thread may be one
parked in a syscall that returns by `sysretq`.

Not observed — the suite is green under KVM on AMD with the RPL fixed, which is
exactly the evidence this entry says is insufficient. Linux's workaround is one
`mov ss, __KERNEL_DS` in its context switch; ours goes in `KernelHw::switch`,
the single site.

Escalation is a killed process rather than a halted machine: vector `0x0C` has a
gate (`StackSegment = 0x0C, stub_ss, error_code` in `kernel/src/arch/idt/mod.rs`).
