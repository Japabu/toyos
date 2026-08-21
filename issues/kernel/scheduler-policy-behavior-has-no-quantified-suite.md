---
status: open
kind: track
opened: 2026-08-20
---

# Scheduler policy behavior has no quantified suite

From the external review of 2026-08-20: the model checking and SMP stress
methodology verify functional correctness; the POLICY — "threads execute,
processes own fair share" — is encoded in implementation intent and validated
nowhere empirically.

`toyos-sched/sim/tests/policy.rs` is the suite, host-side, no guest. Every case
measures and then asserts, and every bound came out as the same expression —
`(runnable threads on one CPU + 1) × (QUANTUM + 2 × RUN_CHUNK)`, the fair band's
insertion-key staleness, which turns out to govern all four:

- **thread-count asymmetry** — one thread against N, both processes pure CPU:
  the single-threaded one finishes exactly `N+1` quanta late at every N from 2 to
  64, and the deficit is unchanged when the window doubles twice. Its share over
  the window it would take against one rival falls from 500‰ to 77‰ at N=64;
  over four times that window it is 453‰ at N=4. So the floor is independent of
  N asymptotically and is a *granularity*, not a per-window guarantee. Negative
  control: per-thread shares take the same figure to 1,020 ms against a 336 ms
  bound.
- **mixed interactive/background** — worst wake 637 ms at 64 hogs, which is the
  spawn burst's stale keys; every other wake in a run is served inside one
  quantum (worst per-run runner-up 8 ms at one CPU, mean 0.36 ms at one hog).
- **wakeup storms** — 64 waiters drain in 27.75 ms on one CPU, 3.1× the
  16-waiter figure: linear in the waiters one run queue holds, and the balance
  path is moving tasks while it happens (43 migrations at eight CPUs).
- **starvation** — a runnable task waits at most one dispatch per rival: 50 ms at
  4 runnable threads, 680 ms at 65, at 0.83–0.97 of the derived bound.

What is left, and what blocks it:

- **Nested `Operation` narrowing.** The type is `kernel/src/scheduler.rs`'s and
  reaches `percpu::cpu_id` and `driver::current_handle`; `kernel/` is excluded
  from the host workspace, so nothing host-side can construct one. The law is one
  line — `Operation::begin` stores `outer.min(until)` and its `Drop` restores
  what it displaced, so an inner establishment can only narrow — and it needs a
  guest gate.
- **Adversarial placement.** Spawn placement is least-loaded-with-rotation and
  the simulator has no knob for anything else, so a lopsided machine cannot be
  staged; what the storm exercises is the idle-probe steal path only.
- **The cost of raising a storm.** `wake_all` claims N waiters in one loop on the
  waker's CPU, and the model's clock does not advance inside a step, so that loop
  is free here. It is a guest measurement.

Two findings the suite records rather than gates. The storm's worst drain *rises*
with machine width — 9 ms at one CPU, 57 ms at eight, with the mean flat at
~3.5 ms — so what the width costs is the tail. And at two CPUs the spawn burst's
residue outlives one interactive wake (30 ms runner-up, against 8 ms at one CPU).
