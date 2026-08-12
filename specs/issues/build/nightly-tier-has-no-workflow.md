---
status: open
kind: defect
opened: 2026-08-11
---

# The nightly tier has no scheduled workflow

`cargo test --test toyos-build -- --nightly` runs the relegated tests manually.
Nothing in `.github/workflows/ci.yml` schedules that command, so the coverage
loss recorded in `specs/test-cost-audit.md` §7 remains absent from automated CI.

Build a scheduled CI job that invokes the nightly tier. Beyond the relegated
tests, the eventual job carries the intended TCG, long-running audio and stress
coverage. It must also merge its shard durations as §7 describes, or the
withheld labels remain frozen at the pre-split baseline and the tier declaration
cannot respond when a Nightly test's CI cost changes.

Task #188 is separate: it holds only the optimisation work that makes relegated
tests cheap enough to return to fast per-PR CI. It does not own this workflow.
