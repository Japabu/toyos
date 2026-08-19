---
status: open
kind: defect
opened: 2026-08-01
---

# The page cache's un-index on a failed fill has no test that can fail

`PageCache::read` now unbinds the slot when the fill fails, so a slot cannot
stay labelled with a block whose read did not happen. **Measured, not
asserted**: with the `self.unbind(slot, block)` line deleted, all three USB
storage tests still pass — 3/3 green in the same session that saw them go red
for a real driver defect. Nothing in the suite drives a *failing* read through
the page cache, because the page cache's device is NVMe and QEMU's NVMe does
not fail a read.

What it would take is a fault-injection actuator on the page cache's own
device, in the shape `i8042-fault` already has: a kernel feature that makes one
read fail, plus an in-guest sequence that fills the cache, forces an eviction
into the failing block, and reads it twice. Two device reads is the assertion —
one means the slot stayed bound and the second reader got the previous tenant.
Roughly 80 lines of kernel and 40 of harness; not built.
