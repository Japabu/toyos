---
status: open
kind: defect
opened: 2026-08-10
---

# A second panic on one CPU throws the first poisoned thread away

`scheduler::POISONED` is one `AtomicU64` per CPU, and `poison_tid` `swap`s into
it (`kernel/src/scheduler.rs:400-412`). The panic-recovery path may store there
and nothing else — it runs with any lock possibly held — and the idle loop's
`reap_poisoned` is the only thing that ever reads it.

So two threads faulting on one CPU before that CPU next goes idle is a thread
lost: never zombified, its joiner never woken, its kernel stack never freed by
the reap path. `poison_tid` says so in the log and then carries on:

```
poison_tid: cpu 0 slot still held 82:0 — its waiter is stranded
```

Found while auditing
`specs/issues/kernel/ring0-jump-to-zero-under-port-polls.md`, where that exact
line is in the report and the fault it accompanies has the register state of a
task resumed from a kernel stack that was freed and reissued under it. The two
may be the same event; nothing has shown that they are.

## Why one slot was enough and is not

One fault per CPU per idle pass is the ordinary case: the faulting thread jumps
into `schedule_no_return`, which reaches `cpu_idle_loop`, which reaps before it
picks another task. The slot only overflows when the CPU takes a *second* fault
before it idles — which needs the first recovery to have left something wedged,
which is exactly the case that matters.

## What it should be

A per-CPU list rather than a slot, bounded by nothing the panic path has to
allocate — an intrusive push onto a `ProcessEntry`'s own storage, or a fixed
array with a count and a loud line when it fills. The constraint that makes this
awkward is the one the comment states: the panic path may hold any lock, so it
may not touch the process table and may not allocate.
