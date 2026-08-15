---
status: open
kind: defect
opened: 2026-08-15
---

# An i8042 interrupt with no byte behind it lands during init, and `i8042_undecoded_bytes` reads it as its own

The driver reports `i8042: 1 interrupts and 0 bytes, nothing decoded — first
seen at 459ms` on a boot where nothing has been typed. `i8042_undecoded_bytes`
then fails, because it finds the **first** line containing `nothing decoded` and
asserts that it names the byte it injected:

```
FAIL i8042_undecoded_bytes: the line names no byte:
  [kernel 0.459 cpu1] i8042: 1 interrupts and 0 bytes, nothing decoded — first seen at 459ms
```

## What it is

An interrupt the ISR finds nothing behind. The likely producer is the driver's
own init: it sends commands to the keyboard and **polls** for the answers, while
the GSI is already unmasked — so a byte can be consumed by the polling read
before the ISR that the same byte raised gets to run. The counters are then
honest and the conclusion the line draws is not: nothing was undecodable, the
byte was simply somebody else's.

The stamps are all inside the keyboard bring-up window: **403 ms, 459 ms and
515 ms** over three occurrences.

## Observed

`cargo test`, dev host, 2026-08-15, on `wt/toyos-logd`:

| tree | full suites | reds |
|---|---|---|
| `origin/main` (`4d8c2e9`) | 7 | 0 |
| this branch before the byte ring went (`b8457df`) | 5 | 0 |
| this branch after it (`ee8369c`+) | 5 | 2 |
| the same with the *drain's* interrupts-off window bounded | 5 | 1 |
| the same with **`write_console`'s** window bounded as well (2026-08-15) | 9 | **2** |
| the tip of the same branch, that one window *not* bounded, same session | 5 | **1** |
| the landing gate: ten suites **back to back**, same tree, loads 6.4–9.7 | 10 | **6** |

**The last row is the most useful thing here and it is a second measurement,
not more of the first.** Same tree, same session, same command — run with no gap
between suites so the host never settles, and the rate goes from about one in
five to six in ten. `71_macro_empty_arg` in the same ten reds once, so it is
this name that moves. A rate that tracks the host is what a bring-up race looks
like, and the harness agreed: every occurrence in that batch came back
`ALONE: GREEN`, which is its name for a test that fails only beside other
guests. Both rows are on `src/redlist.rs`, deliberately as two rows.

## The blame, and it named one aggressor when there were two

**The first four rows above were read as "the drain masks interrupts and
bounding it halves the rate", and that account was incomplete.** L3's review
found a second holder of the same lock with a worse shape: `write_console` took
`BackendGuard` — `cli` plus a global spinlock, with the device write inside it —
once for a **userland-chosen** length, because `SYS_WRITE`'s buffer has no cap
and the byte ring this branch deleted had never held that lock at all. So a
guest doing ordinary console output could mask interrupts for as long as it
liked, on the same machine whose i8042 was being brought up, and that window was
live for every measurement in the table's third and fourth rows.
`specs/log-architecture-spec.md` §8.1 carries the fix; the drain's eight-record
bound and this one are the two halves of what `kernel/CLAUDE.md`'s
`BackendGuard` caveat asks for.

**Bounding it does not move the rate, and that is what settles the blame.** The
last two rows are one session — an interleaved A/B of five suites per arm,
plus the four the landing gate then ran on the bounded arm — and the rate is
where it was: 2 of 9 against 1 of 5, which for counts this size is no movement
at all. The isolated re-run the harness takes afterwards did not even agree with
itself across the two occurrences: `red again` on one, `ALONE: GREEN` on the
other, which is what a race whose window is somebody else's looks like from
here. So what is left is not an interrupts-off window this branch owns; it is
the driver race the two halves below describe, which the branch's timing
exposes and does not cause. Recorded rather than re-run away.

`cargo run -- --known-red i8042_undecoded_bytes` said **NOT ON THE LIST** when
this entry was opened; it now answers the row that cites it —
`src/redlist.rs`, FIRES 3 of 14, dev host loaded, 2026-08-15.

## Two halves, and they want different fixes

- **The driver's**, and it is the real one: an interrupt whose byte the init
  path has already taken should not be counted as an undecodable byte. Either
  init masks the GSI while it polls, or the ISR's "no byte" case is
  distinguished from the "byte that decoded to nothing" case the report is
  about. The second is cheaper and is what the report's own wording already
  implies.
- **The gate's**: `i8042_undecoded_bytes` takes the first `nothing decoded` line
  in the capture and assumes it is the one its injection produced. Any earlier
  one — from boot, from a real spurious interrupt on a laptop — makes it read
  the wrong line. It wants the first line *after* its injection, which it knows
  the time of.

Neither is the log branch's to fix, and the entry says so rather than the branch
carrying a red it did not cause the shape of.
