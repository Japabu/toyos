---
status: open
kind: defect
opened: 2026-08-17
---

# `i8042_health`'s idle-line count is the same vacuous check, one test over

Found while fixing `i8042-quarantine-health-line-count-is-vacuous.md`.
`i8042_health` (`tests/toyos.rs`) has its own copy of the pattern that issue
described and its own copy of the same defect:

```rust
let health = result.serial.matches("sched: cpu=").count();
if health > 50 {
    return Err(format!(
        "{health} idle-health lines — the health verdict is holding a CPU awake"
    ));
}
```

The comment above it says what it is guarding: `verdict_due` (the i8042
health-line deadline check, `kernel/src/drivers/i8042::verdict_due`) has to
self-clear once it has fired, or the idle loop's wake condition
(`crate::drivers::i8042::verdict_due()` in `kernel/src/sched/driver.rs`'s
`execute`) would stay true forever and the CPU behind it would never halt —
"the exact failure the quarantine path already had once", per that comment.

Same root cause as the quarantine issue: `kernel/src/scheduler.rs`'s
`log_health` now rate-limits the *print* to at most once per
`SNAPSHOT_INTERVAL_NS` (10 s) per CPU, so a CPU that spins because
`verdict_due` never self-clears and a CPU that halts cleanly between rare
wakes produce the same number of `sched: cpu=` **lines** — the print fires at
the same rate either way. A regression identical to the one this assertion
was built for would pass it today, for the identical reason.

Not fixed here, filed instead: the current task's mandate was the
quarantine test specifically, and this is a second, separate call site — not
something to fold into that fix without deciding it deliberately. The fix
that issue lands (a per-CPU idle-trip counter in `kernel/src/scheduler.rs`
that is not rate-limited itself, surfaced as `trips=` on the same log line
and read from *that* rather than from a line count) is directly reusable
here: swap `i8042_health`'s line-count check for the same idle-trip-delta
check `i8042_quarantine` moves to, once that lands.
