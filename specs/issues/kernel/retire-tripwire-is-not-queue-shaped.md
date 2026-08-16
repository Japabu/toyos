---
status: open
kind: defect
opened: 2026-08-16
---

# `retire_task`'s tripwire is a constant against a term the workload sets

`kernel/src/scheduler.rs`'s `GIVE_UP` is a `Tripwire` — a constant whose expiry
is a kernel panic. Its own derivation carries a term that is not constant:

> Times `1 + peers`, because one CPU runs one unwind at a time and this victim
> waits out the corpses queued ahead of it. Priced at `peers = 8`: one process's
> threads torn down together onto one CPU.

`peers` is the number of *other* killed tasks in `CpuSched::dying` on the
victim's CPU. Nothing bounds it. A process with more than nine threads whose
teardown lands them all on one CPU exceeds the priced value, and every thread
past the ninth adds another `(1 + peers)`-th of the unwind term — 110 ms each on
a saturated real-time band, 10 ms on an idle one. Past roughly nine simultaneous
teardowns on one CPU the derivation's sum crosses the 10 s constant, and the
panic is reachable from a workload that broke no rule.

The simulator states the same term honestly, because it can: `invariants.rs`'s
`retire_latency_bound` takes `peers` as a parameter and reads it off the run, the
way invariant I5's bound takes the runnable thread count. A wall clock in the
kernel has nothing to read it off.

**This predates bounded deferral and is not caused by it.** The
`(1 + peers) × UNWIND_NS` term entered the sim's I14 at C3+C4's first wave and
the kernel-side derivation never priced `peers` at all until the second. Aging
multiplies the term by 11 under a saturated RT band, which makes the crossing
point closer but does not create it.

Three shapes have been considered and none is this chunk's to choose:

1. **Bound the dying list.** A CPU that already holds *k* corpses refuses the
   *k+1*-th and the retire places it elsewhere. `hand_off` currently refuses to
   migrate a killed task for invariant 7's promptness reason, so this is a
   change to invariant 7 and not to a constant.
2. **Make the wait queue-shaped.** `retire_task` reads the victim CPU's
   `dying_len()` at arm time and scales its own deadline. That turns a
   `Tripwire` into something `kernel/src/time.rs` has no kind for — the type
   deliberately forbids a magnitude with a derivation attached.
3. **Stop waiting.** The wait exists because process teardown frees memory the
   dead thread's page tables still map. `specs/completion-architecture-spec.md`
   §7.4 is where that would be revisited.

Until then the constant is honest about what it does not cover, which is the
whole of what this file records.
