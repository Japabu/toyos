---
status: open
kind: finding
opened: 2026-08-22
---

# `kernel_log_file` reds beside other guests and is green alone, and nobody has a rate for it

Seen on 2026-08-22 while running the nightly names an unrelated change touched
(`cargo test --test toyos-build -- --nightly kernel_log_file`, dev host, merged
`main` 11cc6ef1). One run in four red:

```
FAIL kernel_log_file: logd never opened a file:
[kernel 0.221 cpu0] log-volume: partition mounted, 35651584 bytes ...
  FAIL  kernel_log_file  (2s)
  --- re-running 1 failure(s) alone ---
  [log] 2026-08-22-145930.log: 11757 bytes on the device 21 ms after the ready marker
  PASS  kernel_log_file  (5s)
  ALONE kernel_log_file: GREEN — it fails only beside other guests, so its
  Sched::Parallel is wrong. The run stays red on the classification.
host: fastest boot 2050 ms against the reference 1320 ms — liveness ceilings
paid at 1.55x width
```

`cargo run -- --known-red kernel_log_file` answers `NOT ON THE LIST`.

**The company is recorded, because the runner is the instrument.** A second
agent's `cargo test --workspace` was running in `toyos-banner` on the same
laptop for the red run. Three re-runs immediately after, at load averages 3.12,
3.16 and 3.07 with the same neighbour still present, were **green, 3 of 3**. So
the observation is 1 red in 4 with a widely varying host, and no rate.

The captured red's boot log ends at `log-volume: partition mounted` at 0.221 s,
which is the guest not having reached logd's first write inside the window
rather than a file that was written wrong — but `ALONE: GREEN` is a hypothesis
and not a finding (`tests/CLAUDE.md`), so what is owed is a rate measured in one
session against an unchanged tree, not a re-classification of its `Sched`.

Not the diff it was seen from: that branch's kernel change to this path is the
deletion of an unreachable `if rflags & TF != 0` branch in
`arch::LogCommitGuard::close`, which removes instructions and adds none, and
`kernel_log_file` is Nightly for `Why::Cost` rather than for flakiness
(`src/tiers.rs`).
