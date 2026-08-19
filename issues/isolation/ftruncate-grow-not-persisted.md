---
status: open
kind: defect
opened: 2026-08-01
---

# `ftruncate` to a larger size does not persist on `/home`

`set_len(3 MiB)` followed by `metadata().len()` returns the old length. The same
sequence works on `/tmp`, so this is bcachefs-specific.
