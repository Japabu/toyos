---
status: open
kind: defect
opened: 2026-08-01
---

# `device-test-strategy` requires a `query-pci` verification that exists nowhere

The rule is ground truth at the hardware boundary
(`issues/hardware/device-shape-and-lifecycle-have-no-coverage.md`): what QEMU was *told* to
create must be checked against what the guest enumerated. No such check exists — no test
queries QMP's `query-pci` and compares it against the guest's view. Every profile's device set
is therefore asserted only by the harness's own construction of the QEMU command line, which is
the same source it would be verifying.

Same class as the scheduler entries this was filed beside —
`per-process-fair-split-is-the-policy` and
`granularity-bound-crossed-at-four-widths`: a rule requiring an instrument
nobody built. A third, `sched-check-unreachable-from-build`, stood here until
its instrument *was* built and it was closed; what this names is the class, not
a count. This one matters most for the metal track, where the whole point is that the machine's
device set is not what the harness chose.
