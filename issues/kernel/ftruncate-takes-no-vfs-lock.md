---
status: open
kind: defect
opened: 2026-08-08
---

# `SYS_FTRUNCATE` takes no VFS lock and `SYS_FSYNC` does

Filed out of the check-and-act entry when that closed; one of its three residuals, and the one still live.

`kernel/src/arch/syscall.rs:521` routes `ftruncate` to `ops::ftruncate`
(`kernel/src/object/ops.rs:635`, formerly `fd::ftruncate` in the deleted
`kernel/src/fd.rs`), which calls `file_cache::set_size` under **no** VFS
acquisition, while `fsync` (`object/ops.rs:619`) reaches
`crate::vfs::lock().flush_file(...)`.

The fabricated-zeros write is closed — `flush_file` skips a page `copy_page_out`
says is gone (`kernel/src/vfs.rs`). The window is not: `flush_file`'s
`file_cache::size(file_id)` and `fs.update_metadata(file_id, size, mtime)` are
two steps, so a truncate landing between them records the **older** size. It
self-corrects only because `ftruncate` sets `modified` and the next flush runs —
which is a property of the next caller, not an invariant of this one.

The other two residuals of that entry are not open: `file_cache::read_page` is
fallible now, and the pipe-count direction was declined on the entry's own
reasoning that it buys a named operation and not an unwritable one.
