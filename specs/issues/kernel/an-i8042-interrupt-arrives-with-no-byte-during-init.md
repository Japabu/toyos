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
| the same with the drain's interrupts-off window bounded | 5 | 1 |

So it is a race the log branch's timing moves rather than one it introduces:
what that branch changed here is when the machine goes idle and how long a
console drain masks interrupts, and bounding the second of those took it from
two in five to one in five. `cargo run -- --known-red i8042_undecoded_bytes`
says **NOT ON THE LIST**.

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
