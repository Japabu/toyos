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

## Measured again on `wt/toyos-logd`, 2026-08-15 — and from the other side

`71_macro_empty_arg`, **4 of 14 full suites in one session** — 3 of the 9 with
the change below and 1 of the 5 without, an interleaved A/B of a kernel change
to the console write path
(`specs/log-architecture-spec.md` §8.1's `MAX_CONSOLE_LINE` bound on how long
`write_console` holds the backend) plus the four suites the landing gate ran.
The rate does not move with the change, so nothing about how long that lock is
held moves this. `src/redlist.rs` carries the row.

**The failure was the mirror image of the one above, and that is worth writing
down.** The capture came out *empty* against an expected `17`, rather than
carrying an extra daemon line: the child's own output fell outside
`===TEST_START===`/`===TEST_END===` instead of somebody else's falling inside.
Same window, same race, opposite side. So the shape to look for is "the capture
is not this program's" in *either* direction — a diagnosis that only looks for
added lines will read an empty capture as something else entirely, which is
what happened here before the two were connected.

`specs/log-architecture-spec.md` §4.3 recorded this name at "zero in five" after
that branch bounded its console drain; fourteen suites say roughly one in four
whatever the console lock does, so that was a lucky five, and §4.3 now says so.

No cheap honest fix. The kernel tags its own lines `[kernel `, which is why
those are already filtered; userland writes carry no attribution, so a daemon's
line and the child's are the same bytes on the same fd. Either the child gets
a capture channel of its own (the in-guest runner piping and framing its
stdout, which has to keep the line-by-line liveness `run_test_hooked` depends
on) or console writes gain a writer tag. Both are design calls, not repairs.
