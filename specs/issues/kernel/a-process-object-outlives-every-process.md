---
status: open
kind: defect
opened: 2026-08-10
---

# One `ProcessObject` is left alive per process the machine has ever started

Found by `handle_kill_policy`'s census arm, which is the first thing in this tree
ever to read the per-variant live-object counters. It kills sixteen processes,
reads the count, kills sixteen more and reads it again; the second reading must
not exceed the first, because a lag is constant and a leak accumulates.

Measured on the shared `tests/testcases` boot, 2026-08-10:

```
16 more killed processes left 16 more live objects (113 then 129)
census: PipeRead 2 · PipeWrite 1 · Connection 0 · Device 1 · Acceptor 2
census: IoUring 2 · SharedMem 3 · Connector 2 · Namespace 1 · File 0
census: Console 1 · SysCap 1 · Process 113
```

Exactly one per process, every other variant flat, and **113 is about how many
processes that boot had started by then** — so nothing has been released all
boot, not merely nothing in the last sixteen.

## What is known

- The census counter drops with the object's own `Drop`, so a `Process` counted
  here has a live `Arc<ProcessObject>` somewhere.
- The test's own handle is gone: `Command::status()` drops its `Child`, whose
  `toyos::process::Process` closes on drop.
- `reap_finished` (`kernel/src/process.rs`) removes every entry whose process has
  published, and `reap_poisoned` (`kernel/src/scheduler.rs`) calls it from the
  idle loop, which does run between tests. Removing the `ProcessEntry` drops its
  `object` field.

So either the reap is not reaching these entries, or something else holds the
`Arc`. `ProcessObject::waiters` is an `Arc<KWaitQueue>` from
`sched::waitqs::new_queue`, and whether that registry keeps a reference back is
the first thing to read.

## Why it matters, and why it is not urgent

A `ProcessObject` is small — a pid, a `Lock<Option<Exit>>`, a bool and an
`Arc<KWaitQueue>` — so this is a slow leak rather than a wedge. What it costs is
the property the object model is built on: an object is released when its last
handle goes, and the census exists to say so. It also means
`specs/capability-endowment-spec.md` §8.6's *"per-variant `LIVE_*` census
asserted back to baseline after every churn test"* cannot be satisfied for
`Process` by anything until this is fixed.

`handle_kill_policy` is **red on this today**, deliberately: the gate found it on
its first run and weakening the gate to land the branch would be exactly the
thing the gate exists to prevent.
