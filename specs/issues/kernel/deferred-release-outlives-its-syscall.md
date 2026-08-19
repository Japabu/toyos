---
status: open
kind: defect
opened: 2026-08-19
---

# A deferred release can finish after the syscall that caused it has returned

`object::drain_zero_handles` (`kernel/src/object/mod.rs`) takes the whole queue
and clears `ZERO_PENDING` **before** it runs a single hook:

```rust
let batch = {
    let mut queue = ZERO_QUEUE.lock();
    ZERO_PENDING.store(false, Ordering::Release);
    core::mem::take(&mut *queue)
};
for object in batch {
    object.run_zero_handles();
}
```

So "the queue says empty" is not "the work is done", and the CPU that *queued*
an object is not guaranteed to be the one that releases it. The drain runs at
three sites — syscall exit, `do_schedule` entry, the idle loop — and any of the
other two, on any other CPU, can take a batch out from under the syscall that
filled it. That syscall then reaches its own drain site, is told the queue is
empty, and returns to userland with its objects still unreleased.

**Measured 2026-08-19 on `wt/toyos-fdleak` at `8e9f851`**, `tests/testcases`,
two CPUs, TCG, one binary in the guest. `fd_lifetime`'s holder makes eight
io_uring rings (16 MiB) and is killed; the killer reads `SYS_SYSINFO` eight
times back to back straight after `wait` returns. The deficit against the
pre-spawn reading, in megabytes, over those eight reads:

```
round 1  [12, 10, 10,  8,  6,  6,  6,  6]
round 3  [12, 10, 10, 10,  8,  6,  4,  2]
round 5  [10, 10,  8,  6,  4,  2,  2,  2]
round 9  [14, 12, 10, 10, 10,  8,  6,  4]
round 13 [14, 14, 12, 10, 10, 10, 10,  8]
```

Ten of twenty rounds decayed like that; the other ten read zero on the first
try. It is a 2 MiB staircase — one ring page at a time — which is the other
CPU working through the batch while this one reads.

**Nothing is lost.** Over the same twenty rounds free memory returned to its
starting value every time; the drift against the round-0 baseline was zero at
every round. A kernel trace confirms the shape from the other side: with
`RingRef::drop`, `drain_zero_handles` and `SYS_PROCESS_KILL` logged, all eight
`RingRef` frees land in a `batch=9` drain that runs *after* `kill_process` has
returned, and a second CPU was caught taking a batch mid-kill —

```
[cpu0] KILLPROBE enter target=15 t=552074985
[cpu1] ZQPROBE drain batch=1 t=552533113
[cpu0] KILLPROBE done  target=15 t=554353621
```

## Why it matters beyond a test

The visible consequence today is only that two harness binaries had to learn to
settle (`fd_lifetime`, `shm_release_reclaims`; `specs/issues/build/free-memory-verdicts-share-a-boot.md`
carries that story). The consequence that is not a test is a process which kills
a child to make room and immediately allocates: the pages it just freed are not
free yet, and `SYS_SHM_CREATE`/`io_uring_setup` can answer `ResourceExhausted`
for memory the machine is in the middle of handing back. On a memory-tight
machine that is a spurious refusal, and nothing in the ABI lets the caller tell
it from a real one.

`ops::close_all`'s own doc states the intent this misses — *"Called by exit **and
by kill**, so the drops below are on the path a process taken down by another
CPU follows"* — which is about the drop happening, not about it having finished.

## What to do

Not "drain harder": every drain site already runs, and adding a fourth changes
nothing about a batch another CPU is holding. The two honest shapes are

- **Never publish a batch as absent while it is in flight.** Popping one object
  at a time and clearing `ZERO_PENDING` only when the queue is genuinely empty
  shrinks the window from "every object the kill queued" to "at most one per
  other CPU" — a mitigation, not a guarantee, and at four vCPUs three 2 MiB
  pages is still a visible amount.
- **Give the batch an owner.** The releasing syscall should run the hooks of the
  objects *it* retired, with nothing held, before it returns — which is what
  makes the kill path and the exit path one teardown rather than two, and what
  makes "a killed process holds nothing" a fact rather than a race.

The second is right and it is **not free-standing work**: it is the object
layer's release protocol, which `specs/completion-architecture-spec.md` §21 row
9 already owns and constrains (after C5 no `on_zero_handles` hook may take a
`SleepLock` at all). It belongs with that pipeline, not beside it.
