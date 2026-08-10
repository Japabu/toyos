---
status: open
kind: finding
opened: 2026-08-10
---

# `install_at`'s stated contract is violated at its own call site

`HandleTable::install_at` is `#[must_use]` and says why
(`kernel/src/object/handle.rs`):

> The displaced entry is returned rather than dropped here, so its
> `handle_count` decrement happens where the caller decides — **outside whatever
> guard it is holding**.

`sys_dup2` (`kernel/src/arch/syscall.rs`) calls `drop(displaced)` *inside* the
`with_fd_owner_data` closure, so the decrement happens under the process's own
lock. `File` is an `immediate` row, so a `dup2` over a slot holding the last
handle to a modified file runs `vfs::lock()` + `flush_file` there — a disk round
trip, four ticket spinlocks deep by the root `CLAUDE.md`'s own measurement,
while holding the lock every sibling thread's page-fault handler takes.

Two other sites have the same shape and are safe only by accident:

- `kernel/src/loader/start.rs`'s `drop(displaced)` with the guard alive — the
  displaced entry there is always a duplicate the same call just made.
- `sys_handle_recv`'s batch drop — safe because the lock order happens not to
  invert, not because the site is right.

## Not a deadlock

Checked: every site that holds a VFS guard takes `cwd` out of `ProcessData`
before locking the VFS, so the order is uniformly ProcessData → VFS and there is
no inversion. What this costs is latency on a lock that gates every thread of
the process, on a path a program reaches with one syscall.

## The fix

Hoist the drop out of the closure at all three sites — the pattern the rest of
the file already uses, where the closure hands the value back and the caller
drops it. And `install_at`'s doc becomes true, which is the part that matters:
a doc comment is a claim to verify, and this one is the claim a future caller
would rely on.

`kernel/src/object/ops.rs`'s `open` carries a second instance of the same false
claim — *"re-takes the lock in its `Drop` … with nothing held"* — whose only
caller runs it under `with_fd_owner_data`.
