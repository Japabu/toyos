---
status: open
kind: defect
opened: 2026-08-22
---

# Six `arch::cpu` wrappers are safe `fn` and take a caller-chosen value that reaches hardware

`kernel/src/arch/cpu.rs` draws the line between `pub fn` and `pub unsafe fn`
at "can the caller choose a value that breaks the machine". `write_cr0`,
`write_cr4`, `write_cr3`, `lidt`, `ltr` and `wbinvd` are `unsafe fn` on that
rule. Six others take exactly the same kind of argument and are safe:

| wrapper | what a caller can choose |
|---|---|
| `wrmsr(msr, value)` | any MSR. `IA32_LSTAR` is `SYSCALL`'s only entry point; `IA32_GS_BASE` is where every `gs:` access in the kernel lands; `IA32_EFER` is `NXE`, and clearing it makes bit 63 of every live paging entry reserved instead of a permission |
| `outb(port, value)` / `outw(port, value)` | any I/O port, so any legacy device — including one that can be told to write memory |
| `wrfsbase(val)` | the FS base. `#GP` if non-canonical |
| `invlpg(addr)` / `invpcid(kind, pcid, addr)` | less dangerous than the rest — discarding a translation is the safe direction — but `invpcid` is `#GP` on a `kind` above 3 and `#UD` without the CPUID bit |

Writing the `SAFETY:` justifications for the `arch/` sweep is what found it: every
one of those blocks' comments had to say "the caller chose this and nothing
checks it", which is the sentence `unsafe fn` exists to make unnecessary.

**Not fixed with the sweep because it is a change to code and not to comments.**
`wrmsr` alone has 25 call sites (`rg -c 'wrmsr\(' kernel/src`, 2026-08-22:
`arch/apic.rs` 18, `arch/syscall.rs` 3, `arch/control_regs.rs` 1,
`arch/percpu.rs` 1, `arch/pat.rs` 1, plus the definition); `outb`/`inb`/`outw`
reach across the drivers as well. Marking the six adds an `unsafe` block at
every one of those sites, which is a net *increase* in blocks for a real
increase in honesty — a trade worth making deliberately rather than inside a
documentation pass.

**The exit condition** is that each of the six is `unsafe fn` with a `# Safety`
naming its own fault, or is wrapped by a safe caller that discharges the choice
the way `mm::paging::activate_kernel` does for `Cr3::activate` — `arch::apic`'s
eighteen `wrmsr` calls are all one of five architectural x2APIC registers and
would suit a typed register enum rather than an `unsafe` block apiece.
