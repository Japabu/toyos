---
status: open
kind: defect
opened: 2026-08-08
---

# SMEP is on everywhere except `cargo run`, and nothing asserts it anywhere

Both halves of the old gap closed. The kernel enables it —
`arch/cpu.rs:145-176` `enable_smep` (CPUID leaf 7 EBX bit 7, then `CR4.SMEP`),
with `enable_smap` beside it at `:184-217`, called on the BSP at
`arch/percpu.rs:411-412` and on every AP at `:468-469` — and the harness passes
it, `tests/common/qemu.rs:2199` on both the KVM and TCG arms, landed in `5d53aa0`
(2026-08-06, ancestor of `main`).

Two residuals:

- **`cargo run` was never given it.** `src/qemu.rs:88` and `:90` are still
  `host,+rdrand,+smap,+fsgsbase,+x2apic` and `qemu64,+rdrand,+smap,+fsgsbase,+x2apic`.
  So the interactive path — including `--metal-sim`, whose whole purpose is to
  be the T14's shape — differs from the harness in exactly the dimension the
  harness was changed for. `grep -rn smep tests/ src/` returns one hit, the
  harness argument.
- **Nothing asserts it.** No test reads `smep=on` out of a boot log
  (`grep -rn "percpu: BSP" tests/` → nothing), so deleting the argument, or
  breaking the CPUID gate in `enable_smep`, reds nothing. Nor does any test
  execute a user page from ring 0 — and such a test would be a weak instrument
  anyway, because the kernel executes out of the direct map, which has no U bit,
  so SMEP does not cover the kernel's own alias of a user page. That is
  #159/#166 territory, not this.
