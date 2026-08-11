---
status: open
kind: defect
opened: 2026-08-01
---

# `src/build.rs` cannot enable `sched-check`, so no CI run exercises it

The kernel check build is reachable now — `kernel/Cargo.toml:201` forwards
`sched-check = ["toyos-sched/check"]`, and `cpu::MAX_PASS_NS` is 200 µs with
invariant P asserting against it (`cpu.rs:618`, `:1013`). But nothing in `src/`
mentions `sched-check`, so it can only be turned on by hand and the harness never
does.

A check build nobody can run from CI is halfway back to being unreachable, which
is the defect it was built to fix.
