---
status: open
kind: defect
opened: 2026-08-03
---

# One timed-out test on the shared boot fails every test after it

`run_test` writes `run <name>` to the guest and reads until `===TEST_END`. On a
timeout it returns and the caller moves to the next name — while the guest is
still producing the *previous* test's output. Every later test on that boot then
reads a window that opens on output it did not ask for, and the whole block goes
red on `exit code Some(0)` and `output mismatch`.

Measured 2026-08-03, at the width the wave-4 work was being calibrated at:
`allocator_stress` (1 s alone) exceeded its 5 s ceiling once, and the run
reported **114 failures out of 238** — one real, 110 of them the cascade, and
three unrelated. The tell is a mismatch whose "actual" is verbatim the previous
test's expected output:

```
FAIL c::01_comment: output mismatch
--- expected ---   Hello ×5
--- actual   ---   4 refusal outcomes decoded, none panicked the client
```

It is not caused by parallelism and predates it — anything that makes one guest
test slow enough to time out produces it. What parallelism did was make it
reachable, which is why the shared block is now `Sched::Serial` (`tests/toyos.rs`)
and why this is written down rather than left to be rediscovered.

Fix shape: after a timeout, resynchronise before the next `run` — read until the
timed-out test's own `===TEST_END`, or make the marker carry the test name so a
window that opens on the wrong one says so. Neither is done. Note the second is
strictly better: it detects the desync instead of hoping the drain caught up.
