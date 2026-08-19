---
status: open
kind: defect
opened: 2026-08-07
---

# `exit: <name> pid=N cpu=Xms` is the main thread's CPU, not the process's, and the two lines look identical

`teardown_bookkeeping` prints `cpu={main_cpu_ns}` (`kernel/src/process.rs:1037,
1040`) — the main thread's scheduler total, the same value
`stash_accounting_snapshot` then *adds* `child_threads_cpu_ns` to at :1082
before handing it to `waitpid`. The per-thread line at :1230 has the same shape
and prints that thread's total. Nothing distinguishes them but `pid=` versus
`tid=`.

The result is a process that used less CPU than one of its own threads. From
the 2026-08-07 capture, 43 of the 44 `tone` runs read this way:

```
[kernel  87.632 cpu5 tid=1] exit: tone tid=1 code=0 cpu=121ms
[kernel  87.634 cpu4 tid=0] exit: tone pid=8  code=0 cpu=46ms
```

and doom's three lines (`tid=1` 3409 ms, `tid=2` 14759 ms, `pid=6` 59050 ms)
cannot be added, subtracted or reconciled by a reader who does not have
`process.rs` open. The whole-process figure exists — `waitpid` gets it — and is
the one number the log does not carry.
