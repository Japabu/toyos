---
status: open
kind: finding
opened: 2026-08-01
---

# `rename` refuses an existing destination

FAT gives no way to make a replacement atomic. Deleting the destination first
leaves a window in which neither name resolves, which is worse than an error
the caller can act on — so `Fat32::rename` returns `AlreadyExists`. A VFS
`rename` that wants POSIX overwrite semantics has to do the delete itself and
own that window.
