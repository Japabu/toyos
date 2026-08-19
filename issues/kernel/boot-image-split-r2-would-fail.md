---
status: open
kind: finding
opened: 2026-08-01
---

# `boot-image-split.md`'s R2 refactor would fail the suite as written

R2 proposes removing the USB stick from the machine profiles and adding a virtio device. Both
halves break tests that exist today: three machine tests assert on the USB stick, and the
profile it would add a virtio device to is the one whose defining claim is that it has *no*
virtio device — that is what `--metal-sim` is for.

Not a doc bug — a plan defect. A plan that fails the suite is not ready to execute, and the
suite is right here. Whoever picks R2 up must re-scope it against those tests first.
