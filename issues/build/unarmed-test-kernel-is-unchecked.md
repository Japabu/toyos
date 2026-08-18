---
status: open
kind: finding
opened: 2026-08-10
---

# Nothing checks that the test kernel with nothing armed is the shipping kernel

`specs/assessments/test-cost-audit.md` §5.9.7 replaced 45 kernel builds with two: the one
every image ships and one carrying all 47 actuators, armed by boot parameter.
The actuator kernel's every accessor is a load of a word that is zero unless the
parameter names it, so an unarmed boot of it *should* take the shipped branch
everywhere.

Nothing says so. Before the change, an actuator boot was the shipping kernel
plus exactly one live path and the other 44 paths were not compiled at all; now
they are compiled and skipped, and a skip that is wrong — a `!` dropped, an
accessor called on the wrong actuator, a static left written unconditionally —
would make the whole actuator kernel a different machine.

Two of those were found by reading during the landing itself and are the
evidence this is not hypothetical: `i8042::buffer_full` loaded the fault flag on
every status read and `xhci`'s boot scan stored `BOOT_SCAN_DONE` on every boot,
in both kernels, because the statics lost their `#[cfg]` when the features did
(commit "i8042, xhci: two reads a shipping kernel had no business doing"). Both
were in the *shipping* kernel, where `assert_actuators_match_features` would not
have caught them either — it asks the binary for names, not for behaviour.

What exists today: three tests run on the actuator kernel with nothing armed
(`ACTUATOR_TESTS`, the `SYS_DEBUG` shared boot), so a gross divergence reds them
— but as a broad failure of whichever test was running, which is the class
`CLAUDE.md` warns names the workload and never the cause.

Two shapes that would answer it, neither costed:

- **A differential boot.** Boot both kernels on one profile with nothing armed
  and compare the console, modulo the timestamps and the addresses. Cheap in
  builds — both already exist in a full run — and one extra guest.
- **A rule at the call site.** Every actuator read is `if actuator::x()` guarding
  a block; a lint or a source gate could require that the *shipped* arm is the
  one outside the `if`, which is what makes the fold sound. `src/build.rs`'s
  `declared_actuators` already parses the declaration, so the reader exists.

Filed rather than fixed: the landing it comes out of is already large, and which
of the two is worth a boot is a judgement the next agent should make with the
suite's own numbers in front of it.
