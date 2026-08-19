---
status: open
kind: finding
opened: 2026-08-17
---

# `completion-architecture-spec.md` §21.1 quotes a "Lock order is VFS → here → XHCI" comment in `fat32_adapter.rs`; it is not there

`completion-architecture-spec.md` §21.1 traces the disk lock path and
says the source itself states the order, quoting `fat32_adapter.rs`: *"Lock
order is VFS → here → `XHCI`"*.

That string does not appear in `kernel/src/fat32_adapter.rs`, or anywhere else
in `kernel/src`:

```
$ grep -rn "Lock order is VFS" kernel/src/
(no output)
$ grep -rn "Lock order" kernel/src/
kernel/src/page_cache.rs:10:// Lock ordering: BLOCK_CACHE → BLOCK_DEV (never reversed).
kernel/src/page_cache.rs:25:/// Lock ordering: cache first, then device.
kernel/src/io_uring.rs:10://! Lock ordering: the wake path copies watcher lists under source locks (PIPES,
kernel/src/drivers/i8042/mod.rs:881:/// never run under a driver lock. Lock order is PS2 → KEY_BUF, never the
```

The lock order the trace describes (`vfs::lock()` → `FatVolume::write_at` →
`device(role).lock()` → `UsbBlockDevice::write_blocks` → `xhci::with_disk` →
`XHCI.lock()`) still holds structurally — every function named in the trace
still exists and still calls the next in the chain — but the specific quoted
sentence asserting that the source itself states this order is not backed by
any comment currently in `fat32_adapter.rs`.

Filed as a finding rather than a defect because nothing misbehaves; it is a
quoted-comment citation that needs reconciling (the comment may have been
reworded, moved, or the claim was never literally quoted from a single
comment).

Found 2026-08-17 during a citation-accuracy pass over
`completion-architecture-spec.md`; verified at the tree's tip that day.
