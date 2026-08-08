---
status: open
kind: defect
opened: 2026-08-02
---

# A daemon's boot lines land in whichever test window is open

`run_test` captures every non-kernel console line between `===TEST_START===`
and `===TEST_END===` as the program's stdout, and the C family compares that
whole capture against an `.expect` file. soundd prints `soundd: ready, ...`
and one `soundd: suspended` once, at its own startup, on the same console —
so whichever test is running then absorbs them and fails on output that is
not its own.

Where they land is a race with no fixed answer. At `dbbdcbe` it was
`71_macro_empty_arg`, mid-C-section of a full run. In the full run at
`5d0c5bd` nothing in the C family caught them. **A filtered single-test run is
the worst case, not a cleaner one**: `cargo test -- 90_stdio_buffering` at
`5d0c5bd` fails with `soundd: suspended` prepended to an otherwise byte-exact
capture, because the one window opened is the one soundd's startup falls in.
Judge the C family from a full run, and read a filtered red for *which* line
differs before believing it.

No cheap honest fix. The kernel tags its own lines `[kernel `, which is why
those are already filtered; userland writes carry no attribution, so a daemon's
line and the child's are the same bytes on the same fd. Either the child gets
a capture channel of its own (the in-guest runner piping and framing its
stdout, which has to keep the line-by-line liveness `run_test_hooked` depends
on) or console writes gain a writer tag. Both are design calls, not repairs.
