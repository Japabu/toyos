---
status: open
kind: finding
opened: 2026-08-15
---

# The log storm gate waits for a `done` record its own workload may overwrite

`log_conservation_smp{1,4,8}` boots `log-storm`, which spawns one kernel thread
per shard; each emits `STORM_RECORDS` patterned records and then one
`logstorm done t=… emitted=…`. The in-guest gate reads until **every producer
has said `done`**, and only then computes the conservation law
(`userland/test-runner/src/log_gate.rs`, `kernel/src/log/storm.rs`,
`specs/log-architecture-spec.md` §9.1).

**The `done` record is only safe while one producer writes a shard, and nothing
makes that true.** `sched::driver::placement` picks the least-loaded CPU from a
rotating start (`kernel/src/sched/driver.rs`), so threads spawned back to back
land on distinct CPUs only while every published load is equal — one CPU with a
ready task at that moment sends two of them to the same place. A task is also
stealable between its spawn and its first run. Two producers on one shard means
the second's records lap the first's `done` out of the ring, and the reader then
waits for a record that will never arrive: the gate's 60 s ceiling, in the fast
tier, on the gate the whole log design turns on.

It has not been observed. The comment in `storm.rs` asserted the safety rather
than deriving it, which is what the L4 post-merge review found (its F2).

## What was tried, and why it is not the fix

**A barrier.** Every producer counts itself finished and parks — with a 1 ms
deadline, because a Ring 0 loop that takes no lock is never preempted here, so
two producers sharing a CPU means the second does not run until the first blocks
and a *spin* barrier would deadlock the case it exists to survive — and only
emits `done` once every producer has stopped. That makes the only records after
a `done` the other `done`s and a boot's handful of ordinary lines, against a
shard of 512, which is correct as an argument.

**Measured 2026-08-15 on the dev host: it hangs.** With the barrier in place,
`log_conservation_smp4` passed alone (2 s), passed a whole 259-test suite once,
and then never answered at all in a 12-wide parallel suite — no ceiling report,
no timeout, one QEMU process still alive after fifteen minutes. Whatever the
mechanism is — the timed park not being re-armed on a CPU that never reaches a
pass is the obvious candidate and was not established — a gate that can hang the
suite is worse than a gate with a latent liveness hazard, so the barrier was
removed and this entry opened in its place. The two arms are:

- with barrier: alone 2 s PASS; one full suite PASS; one 12-wide suite HUNG
- without barrier (shipped): alone 2 s PASS; two full suites PASS

## The two shapes that would answer it

1. **A reader that needs no `done`.** The gate already knows the record count
   from the storm's own `start` line and already tracks each producer's next
   index and every gap. "Producer *t* has finished" is then "index
   `STORM_RECORDS - 1` was read, or the ledger accounted for it as lost" — and
   the ledger is what accounts for a lapped `done` too. The termination
   condition becomes "the cursor has caught up and nothing new arrived over *k*
   reads", which is a rewrite of `log_gate.rs`'s verdict loop and touches no
   kernel code at all. **This is the recommended one**: it removes the class
   rather than the instance, and a workload whose liveness depends on a record
   the ring is allowed to drop is the same mistake wherever it appears.
2. **A barrier that is woken rather than timed.** The last producer to finish
   wakes the others by name — the machinery is `toyos_sched::waitq::wake_direct`
   and each thread's `KShared`, which `kthread::spawn` already returns. It keeps
   the current reader and needs the hang above understood first.

Filed rather than fixed: the branch that found it is landing the console line
buffer and `/bin/logd`, and both shapes are a change to the gate the branch is
gated by.
