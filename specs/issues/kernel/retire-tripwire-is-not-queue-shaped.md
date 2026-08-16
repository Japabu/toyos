---
status: open
kind: defect
opened: 2026-08-16
---

# `retire_task`'s tripwire is a constant against a term the workload sets

`kernel/src/scheduler.rs`'s `GIVE_UP` is a `Tripwire` — a constant whose expiry
is a kernel panic. Its own derivation carries a term that is not constant:

> Times `1 + peers`, because one CPU runs one unwind at a time and this victim
> waits out the corpses queued ahead of it. Priced at `peers = 8`.

`peers` is the number of *other* killed tasks in `CpuSched::dying` on the
victim's CPU. Nothing bounds it, and every corpse past the priced eight adds
another `(1 + peers)`-th of the unwind term — 110 ms each on a saturated
real-time band, 10 ms on an idle one. The panic is reachable from a workload
that broke no rule.

**Two things this file previously said about that term are wrong, and both are
corrected here rather than left for the next reader to re-derive.**

*The trigger.* The shape named was "one process's threads torn down together
onto one CPU", and this kernel cannot produce it. `kill_process` and the exit
path both loop over a process's tids calling `scheduler::retire_task`, which
blocks until the victim has been released — so one process teardown holds at
most one corpse at a time on any CPU, however many threads the process has. The
producer of `peers > 0` is *concurrent independent retirers*: separate killer
threads retiring separate victims that happen to share a CPU. That is unbounded
in exactly the way the term needs, so the defect stands; only its stated cause
was wrong, and none of the remedies below is aimed differently because of it,
since all three bound the depth rather than the producer.

*The crossing point.* With fixed terms of 8.02 s and 110 ms per additional
corpse, the sum is 8.02 + 0.110 × N seconds: it equals the derivation's own
priced 9.01 s at N = 9, and first reaches the 10 s constant at N = 18. Nine is
the number of *further* corpses the 990 ms margin buys, not the total count at
which the constant is crossed — the two readings were conflated, understating
the crossing point by roughly a factor of two, and the earlier text stated both
readings in adjacent sentences.

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

**Remedy 2 is the one chosen**, by `specs/scheduling-reservations-spec.md` §8,
and that design answers the objection recorded against it: the scaling factor is
no longer a magnitude with a private derivation attached but a declared
reservation — the per-CPU dying server's guaranteed rate — so the retirer's
deadline is a caller's own arithmetic over a spec-cited rate and a depth it
reads. What `kernel/src/time.rs` genuinely lacks is a *panicking* kind that takes
a citation rather than an absurdity, which that spec records as the one change it
needs there. This file closes when that lands, and not before.

Until then the constant is honest about what it does not cover, which is the
whole of what this file records.
