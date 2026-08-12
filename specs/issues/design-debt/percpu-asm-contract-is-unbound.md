---
status: open
kind: defect
opened: 2026-08-08
---

# The PerCpu asm contract is 47 hand-written `gs:[N]` sites bound to nothing

The owner's own review note, and the one finding in it that names a hazard
rather than a shape: *"this verifies against constants but doesnt gaurantee
that the constants are the same used in for example preempt.rs."* He is right.

`arch/percpu.rs` carries 14 `const _: () = assert!(core::mem::offset_of!(PerCpu,
f) == N)`. Those pin the struct's layout to a set of literals. They do not pin
anything to the assembly, and the assembly is where the offsets are actually
used: measured 2026-08-08, **47 hand-written `gs:[N]` operands across eight
files** — `preempt.rs` 13, `arch/percpu.rs` 9, `arch/syscall.rs` 8,
`arch/idt/mod.rs` 5, `arch/idt/timer.rs` 5, `log.rs` 3,
`arch/idt/device_irq.rs` 2, `arch/idt/tlb.rs` 2 — naming 15 distinct offsets
(0, 8, 16, 24, 136, 140, 216, 224, 232, 240, 244, 248, 252, 256, 260). A third
copy of the same numbers lives in the field comments (`cpu_id: u32, // offset
8`). Reordering a field trips the asserts; changing one asserted literal and
its field together does not, and the 47 asm sites then read the wrong bytes
with no diagnostic at all — on the syscall entry path, the timer stub and the
preemption counter.

Fix shape, agreed in the 2026-08 review (`specs/assessments/code-quality-review-2026-08.md`
§2 arch/, deep dive 1): feed `offset_of!` into the asm as `const` operands. One
source, and all 14 asserts delete with it.

Two smaller PerCpu items ride this and **must not precede it**, because field
surgery before the unification is what the 47 copies punish:

- `lapic_id` (`percpu.rs:81`) has zero readers outside the file — written at
  `:270`, never read. Delete it.
- `alloc_percpu` (`:262`) sets 8 of the struct's 22 fields and relies on
  `alloc_zeroed` for the other 14. One total `ptr::write(PerCpu { .. })` says
  what the struct is. The `current_tid`/`current_pid` `u32::MAX` sentinel stays
  — it is an asm wire format and is already `Option`-decoded at the boundary.
