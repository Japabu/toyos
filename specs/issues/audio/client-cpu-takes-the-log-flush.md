---
status: open
kind: defect
opened: 2026-08-08
---

# The residual T14 underruns are the *client's* CPU taking the log flush, not soundd's

`specs/assessments/metal-logs/2026-08-08-audio-underruns/` is the boot: 54 windows, **686
underruns, 5 drains**, on the tree that already carries
`flush_log_file_if_affordable`. Drains fell 375 → 5; underruns did not.

The correlation runs the wrong way for a soundd defect. Underruns land in
windows where soundd is *on time* (`max_batch=1`, worst wake 461–1130 µs), and
the four windows where soundd was stalled 40–45 ms report underruns 0, 0, 0, 23
— being late gives the client more time, not less. `max_batch=8` with
`underruns=0` also settles the ring's shape: soundd consumed eight client
periods in one cycle and every one was covered, so the ring is normally its full
eight periods = 21.3 ms deep, and an empty one is a producer that stopped for at
least that long. `/bin/tone` is one of the producers that stops and its callback
is a sine, so it is not the client computing; it is the client not running.

**The hypothesis, and it is the log-flush deferral fix seen from the other side.**
`owes_deadline()` asked whether a task parked on this CPU expects a wake *at a
time*. An audio client parks on a pipe with no deadline at all — it is woken by
an event, by an RT daemon that expects it back inside one period — so the test
could not see it, and the flush moved off soundd's CPU onto its client's, where
it costs the same audio. Changed to `owes_wake()`: any parked task at all, with
`LOG_DEFERRAL_CEILING_NS` unchanged. The T14 reports CPUs at `parked=0`
throughout this log, so there is somewhere for the flush to go.

**Unverified, and only the owner's next boot can verify it** — QEMU's `/log` is
a fast virtual disk and the whole audio family reports `underruns=0` on this
host either way. What to read on that boot:

- `starve_max=` is new on soundd's stats line: the longest unbroken run of
  underrun periods. Near 1 with a large `underruns` kills the hypothesis
  outright — that is a client missing by a hair, not one that stopped. 8–20 is a
  stall of 21–53 ms, which is the flush.
- `max_wake_lat_us`'s cluster should move from ~2690 to ~2902 µs, because it is
  one device period and the period is now the right one.
- Everything should sound a tone and a half lower than it did.
