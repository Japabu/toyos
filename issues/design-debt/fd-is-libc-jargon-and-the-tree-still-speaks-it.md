---
status: open
kind: defect
opened: 2026-08-19
---

# "fd" is libc jargon, and the rest of the tree still speaks it

Owner ruling, 2026-08-19: **"fds belong only in libc jargon."** The kernel has
no file descriptors — `kernel/src/fd.rs` is deleted, `toyos-abi/src/handle.rs`
is the vocabulary, and a process holds typed handles. POSIX's integer fd is the
interface of exactly one layer, `userland/libc`, and the word is correct there
and nowhere else. Ruled with it on 2026-08-20: the string "iouring" names a
Linux mechanism this kernel does not implement, and goes the same way — the
mechanism is an **inbox**, and SQ/CQ/SQE become submission ring, completion
ring and submission.

**The wave has landed, in three pull requests.** PR-B took the tree-local half.
PR-A's finale took `toyos-abi`, `toyos`, every forced call site, the two
registered names `abuse_inbox` and `inbox_cancel_wakes`, the `rust` submodule
gitlink and both fork pins, in one merge across four repositories. What is left
is below, and none of it is the sweep.

## What is still owed

- **The kernel's internal vocabulary.** `kernel/src/io_uring.rs` →
  `kernel/src/object/inbox.rs`, `IoUringObject` → `InboxObject`, `IoUringOp`,
  `IoUringInstance`, `RingId`/`RingRef`, `sys_io_uring_setup`/`sys_io_uring_enter`,
  and the 21 `io_uring_watchers` accessors across `pipe.rs`, `keyboard.rs`,
  `mouse.rs`, `net.rs`, `log/user.rs`, `object/port.rs` and the two audio
  drivers. Tree-local, needs no sysroot window, and deliberately left out of
  the finale so that landing held the machine's sysroot for one merge. The
  header of `kernel/src/io_uring.rs` says the same thing where a reader of that
  file will hit it. `KObjectRef::Inbox` already moved, because the variant's
  name *is* `OBJECT_KINDS[5]` and `CENSUS_KIND` asserts the two agree.
- **The two renamed tests' prices.** `abuse_inbox` and `inbox_cancel_wakes`
  carry `UNMEASURED_MS` in `tests/test-durations`. The marker buys exactly one
  measured run; the next commit replaces both with that run's own
  `test-durations-merged` values — never a re-run (`src/durations.rs`).
- **Two `CLAUDE.md` lines, which an agent may not edit.** `kernel/CLAUDE.md`
  still names `remove_fd`, which has been `cancel_by_source` since PR-B;
  `userland/CLAUDE.md` still says "fds".
- **`std::os::toyos::io::{AsRawFd, FromRawFd}`.** The naming table ruled that
  `os::toyos::*` is ToyOS's own extension API and must speak ToyOS, and named
  every `process` row — but not this one. std's *POSIX* surface keeps the word
  by charter; whether an `os::toyos` trait with a POSIX name is that surface or
  the extension API is unsettled, and `tests/toyos-rust-tests/src/bin/std_fs.rs`
  is its only caller in this repository.
- **Four issue files cite the deleted `kernel/src/fd.rs`**, one of them by line
  number: `issues/filesystem/close-cannot-report-io-error.md`,
  `issues/kernel/ftruncate-takes-no-vfs-lock.md`,
  `issues/kernel/sys-read-doc-comments-describe-nothing.md` and
  `issues/audio/disk-wait-pins-a-cpu.md`. Each belongs to its own entry rather
  than to this one; the file they name has not existed since the ruling.

## The closing check, and what it is allowed to find

`git grep -Inwi -e fd -e fds` outside `userland/libc` — the tree, excluding
`rust/`, lockfiles and `tests/testcases/` (third-party C). Run at `fa710af`,
it returns 88 lines, and **twelve of them are code**:

| site | why it keeps the word |
|---|---|
| `src/buildlock.rs` (5) | `flock(fd: i32)` on the *host*, through `std::os::unix::io::AsRawFd`. A genuine POSIX descriptor. |
| `src/qemu.rs` (2), `tests/common/qemu.rs` (2) | `ovmf/*.fd` is a filename extension. `NOTICE` names the same six files. |
| `src/toolchain.rs:2378` | a rustc diagnostic path, `library/std/src/os/fd/owned.rs`, quoted verbatim. |
| `src/redlist.rs` (2) | this file's own path, and the record of `fd_lifetime` → `handle_lifetime`. |
| `userland/console/src/main.rs:25`, `userland/terminal/src/main.rs:10` | `use std::os::fd::AsRawFd;` — std's POSIX module, on the charter `userland/libc` has. |

Everything else the check finds is `issues/` prose and `NOTICE`. An issue
reports a defect in the words it was reported in, and the tracker is not code;
the four stale `kernel/src/fd.rs` citations above are the exception and are
listed as work.

**The check is weaker than the ruling, and anyone re-running it has to know
that.** `\bfd\b` matches neither `fd_map`, `ring_fd`, `poll_add_fd`, `PipeFds`
nor `FDs`, because `_` and a trailing letter are word characters. The wider
pattern is

```
(?i)(^|[^a-zA-Z0-9])fds?([^a-zA-Z0-9]|$)|(?i)(^|[^a-zA-Z0-9_])fds?_|_fds?($|[^a-zA-Z0-9_])|[a-z]Fds?($|[^a-zA-Z])
```

and over the same tree minus `issues/` and `NOTICE` it returns 39 lines at
`fa710af`, of which the surplus over the table above is `0xFD`, `\u{FFFD}` and
one abbreviated commit hash — plus the `as_raw_fd` calls in console, terminal
and `std_fs`, which are the std sites the rows above already name.

## `inbox` names three things, and two of them are meant to converge

`completion::Inbox` (`kernel/src/completion/inbox.rs`) is a *task's* bounded
record ring, with `kernel-loom/tests/inbox.rs` and two cargo features named
after it. The ABI's inbox is the object userland sets up, and the track's own
chunk list says "the ring as an inbox": they are meant to become one thing, so
the kernel's object lands at `crate::object::inbox` beside
`completion::inbox` rather than as a third `Inbox`. That is also why the SDK's
`Poller` was **not** renamed to `Inbox` — a third one in userland would undo
the separation. `ConnectionEnd`/`PortShared`'s `inbox`/`outbox` are the common
noun and are unrelated.
