---
status: open
kind: defect
opened: 2026-08-03
---

# `Command::output()` returns an empty stderr, always

`sys::process::toyos::output` (`rust/library/std/src/sys/process/toyos.rs:235`) reads the
stdout pipe and then returns `Vec::new()` for stderr unconditionally. It has already asked
`spawn` for a stderr pipe, so the bytes exist and are dropped — and a child that writes
more than the pipe holds blocks forever against a reader that never comes.

`Output::stderr` is a documented promise this does not keep, which is the sentinel problem
in another dress: the caller cannot tell "the child said nothing" from "we did not look".
Measured: `/bin/cp` refusing a missing source issues three `SYS_WRITE`s to fd 2 and
`output().stderr` comes back empty.

`wait_with_output()` is the cross-platform path and does read the pipe, so the workaround
is to `spawn()` and call that — which is what `toybox_file_tools` does, one stream at a
time to stay off the two-pipe `read2` path. The fix is for `output` to read both pipes, or
to be deleted so the cross-platform default is used.
