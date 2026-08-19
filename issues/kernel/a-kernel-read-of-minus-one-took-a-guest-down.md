---
status: open
kind: finding
opened: 2026-08-15
---

# A kernel-mode read of `0xffffffffffffffff` took one guest down, once

One twelve-wide dev-host suite, 2026-08-15, on `wt/toyos-logd56` at `19ce5d0`:

```
FAIL i8042_fadt_denial: kernel panic: [kernel 0.398 cpu1] KERNEL PANIC: read unmapped address at 0xffffffffffffffff
```

**That is the whole capture.** `run_test_paced` returns the moment it sees
`KERNEL PANIC`, so the report's own body — the page walk, the fault trace, the
backtrace — is on the wire after the harness has stopped reading and is not in
the log. Nothing else about the run is unusual: the phase's fastest boot was
1,370 ms against the 1,320 ms reference, and the guest was one of twelve.

`0xffff_ffff_ffff_ffff` is `-1` widened, or `u64::MAX`, read in **kernel** mode
on cpu1 at 398 ms — before userland on that config. It is not
`issues/kernel/echo-faulted-after-the-fault-arms.md`, which is a *Ring 3*
segfault at address `1` in a spawned `/bin/echo`; this one is the kernel
dereferencing a sentinel of its own.

## What is known about the rate, which is very little

- **1 of 7** twelve-wide suites at `19ce5d0`, and **ALONE GREEN** on the
  harness's own re-run — *"it fails only beside other guests, so its
  `Sched::Parallel` is wrong. The run stays red on the classification."*
- **0 of 7** at `a76ffd0` in the same session, and 0 of 7 in the session before
  it on the same tree.
- No `src/redlist.rs` row: one observation with no reproduction is not a rate,
  and `cargo run -- --known-red i8042_fadt_denial` correctly says the name has
  never been measured.

**The name is the workload and not the cause.** `tests/CLAUDE.md`'s rule applies
exactly: a machine-wide panic reds whichever test was running. `i8042_fadt_denial`
boots its own guest with an actuator that makes the FADT deny the controller, so
the i8042 path is the most likely neighbourhood — but 398 ms on cpu1 is also
where AP bring-up, xHCI enumeration and the boot's device probes are, and
nothing here separates them.

## What the next observation needs

The panic body, which the harness threw away. The cheapest way to get it is to
re-run the name under `cargo test --test toyos-build -- i8042_fadt_denial
--nocapture` in a loop and read the serial directly, or to have `run_test_paced`
keep draining for a moment after a `KERNEL PANIC` line so the report that
follows the marker is in the failure message — a panic whose backtrace is
unreadable costs more than the milliseconds of drain it would take to keep it.
The second is a harness change and belongs with whoever owns this, not with the
log branch that saw it.

Seen while measuring PR #82's storm-gate fix. Nothing on that branch touches the
i8042 path, the FADT, AP bring-up or any kernel control flow — its kernel diff
is comment text plus the deletion of one function that had no callers.
