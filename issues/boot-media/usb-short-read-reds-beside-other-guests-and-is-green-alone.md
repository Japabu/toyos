---
status: open
kind: finding
opened: 2026-08-22
---

# `usb_short_read` reds beside other guests and is green alone, and nobody has a rate for it

Seen on 2026-08-22 in a full fast tier on the dev host (`main` 7129931d merged),
one run in three:

```
FAIL usb_short_read: one short read cost the disk the rest of its sweep
  FAIL  usb_short_read  (5s)
  ALONE usb_short_read: GREEN — it fails only beside other guests, so its
  Sched::Parallel is wrong. The run stays red on the classification.
```

`cargo run -- --known-red usb_short_read` answers `NOT ON THE LIST`.

Re-run immediately after, alone, on the same tree and in the same session:
**green, 2 of 2**, 2.9 s each, host load average 7.70 with another agent's work
on the machine. So the observation is 1 red in 3 with no rate behind it.

`ALONE: GREEN` is a hypothesis and not a finding (`tests/CLAUDE.md`): what is
owed is a rate measured in one session against an unchanged tree, not a
re-classification of its `Sched`. The verdict's own words — one short read
costing the disk the rest of its sweep — are about the driver's recovery after
`usb-short-read` injects the under-delivery, so a red here is worth a look at
whether the sweep's continuation depends on timing the host can stretch.

Not the diff it was seen from: that branch changes `syscall_window_nmi`'s
assertions and two kernel doc headers, and touches nothing under `xhci/` or
`usb_gate`.
