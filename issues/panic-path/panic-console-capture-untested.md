---
status: open
kind: defect
opened: 2026-08-01
---

# Nothing distinguishes `panic_console::capture` from a no-op

`capture`/`discard_capture` (`drivers/panic_console/mod.rs:362`, `:374`) have no
test that would fail if they stopped working. Measured, not assumed: with
`capture`'s body replaced by `return`, `screen_late_panic` still passes — and
`main.rs` claimed that test was "the one test that fails if the capture stops
happening". The claim was false; it has been corrected in the code.

An open **testing** gap, not a code defect. The functions were kept for a
narrower surviving reason — freezing the report at the panic instant, where
`live_tail` re-reads a ring that siblings running with IF=0 are still writing
to — and carry a comment saying explicitly not to delete them on the grounds
that the tests pass.

Another gate that cannot fail, and the third found this session, after I5
fairness and the unreachable kernel `check` build.
