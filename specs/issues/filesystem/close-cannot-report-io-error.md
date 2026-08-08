---
status: open
kind: defect
opened: 2026-08-08
---

# `close` cannot report `EIO`

Filed out of the `vfs::FileSystem` error-channel entry when that closed; the chain is honest everywhere except here.

`Drop for OpenFile` (`kernel/src/fd.rs`) logs `warning: flush failed on close`
and has nothing to return, so a process whose last write the device refused is
**never told**. Every other way of asking is honest — `fsync`, and a `write`
whose page was refused — and no doc comment anywhere says `close` is not.

That is the whole defect: not that `close` cannot fail, but that the one call
that cannot report it does not say so.
