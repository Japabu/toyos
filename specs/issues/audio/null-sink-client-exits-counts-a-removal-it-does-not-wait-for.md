---
status: open
kind: defect
opened: 2026-08-15
---

# `null_sink_client_exits` counts a soundd line whose arrival it does not wait for

```
FAIL rs::null_sink_client_exits: soundd reported 1 client removals, expected 2
```

PR #85, run `31904338273`, job `95059750268` (`guest (1)`), on a branch of
documentation and one data table. `ALONE: GREEN, and it was alone both times —
nothing the harness controls differed, so it failed once and passed once. That
is a rate and not a classification.`

**The capture says which line went missing and why it could.** Round 1's
removal arrives before the guest's own round-1 sentence; round 2's never arrives
at all, because the guest finishes and the window closes:

```
soundd: client 0 connected (id=0)
[kernel 7.135 cpu0] exit: tone pid=93 code=0 cpu=0ms
soundd: client 0 removed (closed)
round 1: tone exited after 1.11s
soundd: client 0 connected (id=1)
[kernel 8.229 cpu0] exit: tone pid=94 code=0 cpu=0ms
round 2: tone exited after 1.10s
null sink drained two clients in series
[kernel 8.249 cpu1] exit: test_rs_null_sink_client_ex pid=92 code=0 cpu=9ms
```

Round 1 had a whole second round to run before the window ended; round 2 had
the guest's own exit. Nothing in the test waits for soundd to speak.

**The check contradicts its own doc comment.** `check_null_sink_client_exits`
(`tests/toyos.rs`) says "The race is scheduling and this test does not try to
win it; what it asserts is that neither outcome of the race is worded as a
death" — and then calls `audio::check_departures(&result.serial, 2)`, whose
`expect` is an exact count and whose own doc gives the reason: "a capture where
no client ever left would otherwise satisfy every check above it vacuously".
Both statements are right. What is wrong is that non-vacuity is being bought
with a count over a window the test does not control, so the assertion that was
never meant to race is the one that reds.

Two shapes that keep the teeth, and neither is a looser count:

- **Wait for the second removal**, on the guest's own liveness rather than on
  wall clock — the shape `metal_sim_null_audio`'s retirement took when it
  stopped reading soundd's first line through a span of host time
  (`src/redlist.rs`, that row).
- **Buy non-vacuity somewhere that is not a race**: soundd's own
  `clients=` statistic is in the same capture, and a run in which no client
  ever left cannot show it returning to zero twice.

What must not happen is `expect: 1`, or a range: the departure vocabulary this
checks — that no removal is worded as a death soundd did not establish — is a
defect that was real at 5 of 44 runs, and it is asserted per removal.
