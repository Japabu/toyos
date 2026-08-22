---
status: open
kind: finding
opened: 2026-08-22
---

# Five `unsafe { asm!("cli"/"sti") }` blocks that `arch::cpu` already has a safe name for

`kernel/src/arch/cpu.rs` has carried

```
#[inline] pub fn enable_interrupts()  { unsafe { asm!("sti", options(nomem, nostack)); } }
#[inline] pub fn disable_interrupts() { unsafe { asm!("cli", options(nomem, nostack)); } }
```

since before this entry, and five sites spell the same instruction with the
same option set inline instead. Each is one `unsafe` block that stops existing
by naming the function, with **no instruction changed** — the guest profile is
`opt-level = 2`, so `#[inline]` leaves nothing behind:

- `kernel/src/arch/idt/mod.rs:405` (`sti`), `:407` (`cli`) — the exit-to-user
  preempt bracket.
- `kernel/src/arch/idt/mod.rs:454` (`cli`) — the page-fault arm, whose
  *matching* `enable_interrupts()` two lines above is already the helper. The
  inconsistency is inside one match arm.
- `kernel/src/main.rs:103` (`cli`) — the panic handler's first statement.
- `kernel/src/main.rs:720` (`cli`) — `pre_idle_wedge`, behind `boot-actuators`.

`sched/` had three more of exactly this shape and they are gone
(`issues/build/clippy-has-never-run-here.md`'s sweep records the measurement:
the whole kernel's emitted assembly at `[profile.toyos]` was byte-identical
across that substitution, 225,014 instruction lines, and a deliberate
`enable`→`disable` mutation moved exactly one of them).

Left rather than fixed because `arch/` and the kernel's root files are two
other areas' sweeps and a rule of this project is not to reach into them. The
sweep that reaches either should take these with it; there is nothing to
decide, only to do.
