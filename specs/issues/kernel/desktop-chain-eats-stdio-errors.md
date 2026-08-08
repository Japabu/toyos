---
status: open
kind: defect
opened: 2026-08-06
---

# The desktop chain reads every stdio error as end-of-input, and says nothing

Found while tracing #156's teardown, where both of these had to be worked
around before the cause could be read at all.

`userland/shell/src/main.rs`'s `read_byte` is `read_exact(&mut buf).ok()?`, so
a device error, a revoked fd and a genuine EOF are one value: `None`. `readline`
turns that into `break`, `main` returns, and the shell exits **0**. A shell that
died because its terminal vanished and a shell whose stdin failed are
indistinguishable from outside, and both look like a clean exit.
`userland/terminal/src/main.rs` has the same shape twice —
`shell_stdout.read(&mut buf).unwrap_or(0)` for stdout and stderr, where `0` is
its own signal to leave.

Neither says anything on the way out, and the channel that would carry it is
the one that failed: the terminal breaks on stdout EOF **before** it drains
stderr, so the shell's last stderr line is dropped with it. What established
the cause was encoding `io::ErrorKind` into the shell's exit status, because
the kernel's `exit: name pid=N code=C` line is the one record neither end can
swallow.

No reproduction of a non-EOF error here; the one traced was a real
`UnexpectedEof`. The defect is that there could be, and nobody would know.
A fix is a message naming the fd and the kind, and a stderr drain before the
terminal leaves.
