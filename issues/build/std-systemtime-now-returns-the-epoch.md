---
status: open
kind: defect
opened: 2026-08-15
---

# std's `SystemTime::now` returns 1970 on a machine whose libc knows the date

The std fork never asks the kernel for the wall clock:

```rust
// rust/library/std/src/sys/time/toyos.rs
pub fn now() -> SystemTime {
    // No wall clock — return epoch
    SystemTime(Duration::from_secs(0))
}
```

The comment is false about the tree it is in. `SYS_CLOCK_EPOCH` is syscall 75
(`toyos-abi/src/syscall.rs:87`) with a wrapper at `:995`, and
`userland/libc/src/time.rs:49` calls it and gets the real answer:

```rust
toyos_abi::syscall::clock_epoch().map_or(-1, |secs| secs as i64)
```

So on one machine a C program prints the correct date and a Rust program prints
1970-01-01. Every std consumer of the wall clock inherits it: file mtimes
compared against `SystemTime::now`, any elapsed-time-since-epoch arithmetic, and
anything that logs a timestamp through std rather than through the kernel's
record.

`kernel/src/clock.rs:16-19` states the kernel's position — *"Nothing here invents
1970"* — which is what makes this a divergence rather than a shared limitation.

## Scope

`Instant` is fine and is not part of this: it is monotonic and separately
sourced. Only `SystemTime::now` is wrong, and it is wrong by omission — the
syscall it needs exists, is stable, and has a caller two directories away.

Verified at rust submodule `87971e6d0ed0`; the file is
`rust/library/std/src/sys/time/toyos.rs`, and the fork rules are
`specs/forks.md`.

## What a fix owes

Reading `clock_epoch()` in `now()` is the whole change. What it also owes is a
gate: `rg`ing the tree found no test that compares a std timestamp against a libc
one, and the two disagreeing by 56 years went unnoticed. `wall_clock_now` and
`wall_clock_file` exercise the kernel side; neither reaches std's
`SystemTime::now`.
