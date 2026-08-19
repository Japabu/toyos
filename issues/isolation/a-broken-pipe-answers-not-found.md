---
status: open
kind: defect
opened: 2026-08-10
---

# A write into a pipe whose reader is gone answers `NotFound`

`ops::write_pipe` maps `pipe::PipeWrite::BrokenPipe` to `SyscallError::NotFound`
(`kernel/src/object/ops.rs`). The handle resolved, the object is there, and the
fact being reported is that the *other end* has closed.

That is the one thing `NotFound` is not allowed to mean. The rule is stated in
the root `CLAUDE.md`, about the same error word on the same syscall family:
*"`NotFound` means the name is not there and nothing else may say so, which is
why `open` with `CREATE` acts on `NotFound` alone."*

And there is already a word for it. `SyscallError::Gone` was added by the
endowment branch's chunk 3 and its `Display` is literally *"the other end is
gone"*. `SYS_NAMESPACE_OPEN` answers it for a port whose acceptor has been
dropped; a connection whose peer end has been dropped answers `NotFound` for the
same fact.

The second arm of `connect_before_serve` is specified as *"the client's write
must return `Gone`"*. It returns `NotFound`, and the test asserts what the kernel
says rather than what was asked for.

## What changing it touches

Small and not zero, which is why the endowment branch filed it rather than doing
it on its last verification cycle:

- `kernel/src/object/ops.rs`, one arm.
- `userland/soundd/src/main.rs`'s `signal_clients`, whose client-death detector
  is `matches!(write_nonblock(..), Err(SyscallError::NotFound))` and whose doc
  comment names the word. **This is the risky one**: it is gate A's path.
- `rust/library/std/src/sys/pipe/toyos.rs` and `sys/stdio/toyos.rs` need
  `Gone => io::ErrorKind::BrokenPipe`, which is what POSIX callers expect and
  what `NotFound` is not. `stdio/toyos.rs` already has that arm.
- `userland/libc/src/posix_io.rs` maps `NotFound` to `ENOENT`; a broken pipe
  should be `EPIPE`.

Nothing outside those reads the word off a write.
