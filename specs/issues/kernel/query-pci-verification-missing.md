---
status: open
kind: defect
opened: 2026-08-01
---

# `device-test-strategy` requires a `query-pci` verification that exists nowhere

The strategy's rule is ground truth at the hardware boundary: what QEMU was *told* to create
must be checked against what the guest actually enumerated. No such check exists — no test
queries QMP's `query-pci` and compares it against the guest's view. Every profile's device set
is therefore asserted only by the harness's own construction of the QEMU command line, which is
the same source it would be verifying.

Same class as the three scheduler instruments below: a spec requiring an instrument nobody
built. This one matters most for the metal track, where the whole point is that the machine's
device set is not what the harness chose.
