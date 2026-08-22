---
status: open
kind: defect
opened: 2026-08-22
---

# The first six instructions of `syscall_entry` run on the user's stack

`SYSCALL` changes CPL and `RIP` and nothing else — it does not switch stacks —
so `kernel/src/arch/syscall.rs`'s `syscall_entry` runs at CPL 0 with `rsp` still
pointing into the process's stack until `mov rsp, gs:[16]`. Counting
`ring3_naked_asm!`'s `cld`, that is six instructions.

**An exception taken in that window builds its frame on a user page from CPL 0,
which SMAP refuses, and the `#PF` lands on the same stack and escalates to
`#DF`.** That is a machine halt, and it is not hypothetical: it was measured on
this tree on 2026-08-22 with `TF` missing from `IA32_FMASK` —
`DOUBLE FAULT on CPU 1`, `rip=…syscall::syscall_entry+0x0`,
`rsp=0x000000fffffffc30`, `cr2=0x000000fffffffc28` (`rsp - 8`, the first qword
of the frame the CPU pushes) on a page the report's own walk shows as
`P=1 W=1 U=1`. Three Ring 3 instructions reached it.

The `TF` route is closed — the mask names the bit now, and `debug_trap`'s
`tf-syscall` arm is the gate — but the *window* is what made one bit fatal, and
it is still open for everything else that can land there. What is left:

- **NMI.** `arch::idt`'s table gives vector 2 a `ring0` direct gate with no IST,
  so an NMI in the window has the same shape. Reachable today only from
  `sched::dump`, which the owner sends with Ctrl+Alt+D — a diagnostic that can
  halt the machine it is diagnosing, on a window six instructions wide.
- **`#MC`.** An abort, so the machine was going down anyway; it would go down
  without its report.

The architectural answer is an IST for the vectors that can arrive there, which
puts the frame on a kernel stack whatever `rsp` holds. `#DF` already has IST1
(`arch::idt`'s `ist 1` column) and `percpu::alloc_ist1_stack` is the allocator;
`#DB` no longer has a handler of its own, and vector 2 and vector 18 are the two
rows that would gain one. The cost is that an IST stack is not re-entrant, which
is why `#DF`'s is a stack nothing returns from — an NMI that can nest needs more
than a column change, and that is the part nobody has designed.
