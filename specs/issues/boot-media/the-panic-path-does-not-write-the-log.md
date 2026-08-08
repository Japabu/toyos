---
status: open
kind: rejected
opened: 2026-08-01
---

# The panic path does not write the log, deliberately

Not a gap to close later: `log_file`'s module documentation states the argument.
A panic-time flush needs the sink lock, the VFS lock, the file cache lock, the
heap, the log volume's device lock and the xHCI lock, and a panicking thread may hold any
of them — so it would deadlock in precisely the cases the log exists for. The
second half of this argument used to be that a torn FAT write leaves the volume
holding `BOOTx64.EFI` and `kernel.elf` unbootable; with the log on its own
partition that is gone, and the worst a half-finished write costs is the
diagnostic itself. The lock argument stands alone. The panic path keeps the
on-screen console, which takes no lock at all. What the file has after a panic
is everything up to the last idle pass.
