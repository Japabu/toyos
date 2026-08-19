---
status: open
kind: defect
opened: 2026-07-31
---

# Nothing the kernel logs on the shutdown path ever reaches the console

The same mechanism as `log-ring-flushes-one-line-behind`, with a harder ending. `SYS_SHUTDOWN`
(`syscall.rs:219-224`) logs "Syncing filesystems...", syncs, logs
"Shutting down." and calls `acpi::shutdown()`. Both lines go into the ring and
the power goes off before anything drains it. Measured on the MetalDisk profile:
the last console line of a clean shutdown is the kernel's `spawn:` line for
`/bin/shutdown`, and QEMU exits shortly after.

So a shutdown that panics or hangs mid-sync produces no diagnostic at all —
including on the T14, where writing back is the operation with something to
lose. `nvme_large_device` had to assert on the disk image host-side instead,
which is a better assertion anyway but was not a free choice. Fix is the same
flush-before-parking as the idle case, plus an explicit drain before
`acpi::shutdown()`.
