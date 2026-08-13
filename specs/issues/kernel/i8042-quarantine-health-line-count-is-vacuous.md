---
status: open
kind: defect
opened: 2026-08-13
---

# `i8042_quarantine`'s second assertion can no longer be tripped

Found while fixing the i8042 family's fixed 5 s collection deadline
(`tests/toyos-rust-tests/src/bin/i8042_keyboard.rs`) — not fixed here, filed
per the instruction that found it.

`i8042_quarantine` (`tests/toyos.rs`) reds if `sched: cpu=` appears more than
50 times in the captured serial after the flood-quarantine fires:

```rust
let health = result.serial.matches("sched: cpu=").count();
if health > 50 {
    return Err(format!(
        "{health} idle-health lines after the quarantine — a CPU is spinning, not halting"
    ));
}
```

The comment beside it says why: the first version of the quarantine driver
left the `irq_ring` record undrained after quarantine, and a spinning CPU
printed 2685 of these lines in 5 s against 1 on a healthy run — a real defect,
caught by a count that could actually distinguish the two.

`kernel/src/scheduler.rs`'s `log_health` no longer allows that signal to
exist. It replaced the earlier one-line-per-1000-idle-trips counter with a
per-CPU wall-clock rate limit, `SNAPSHOT_INTERVAL_NS = 10_000_000_000` (10 s):
each CPU may print `sched: cpu=` at most once per ten seconds, however many
times it passes through idle in between. `i8042_quarantine` boots
`Profile::Metal`'s default `smp: 2` under a 30 s `run_test_hooked` ceiling, so
the hard maximum achievable is `2 CPUs × 3 windows = 6` lines — nowhere near
50, and nowhere near it regardless of whether the quarantine path spins a CPU
or halts it cleanly. A regression identical to the one this assertion was
built for would pass it today.

The assertion is not wrong about what it wants to know — "a keyboard, not a
CPU" is still the right claim — it is reading a counter that can no longer
carry the answer. A fix needs a signal the 2026-08-13-and-later scheduler
still produces at a rate that separates spinning from halting: a direct read
of run-queue occupancy or CPU utilization, or a diagnostic dedicated to this
test rather than the idle loop's own rate-limited health line.
