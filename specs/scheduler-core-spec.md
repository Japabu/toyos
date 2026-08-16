# Scheduler

## 1. Definitions

- A **scheduling pass** runs on a CPU at interrupt exit, at syscall exit, when
  the running task blocks or exits, and in the idle loop. Cross-CPU messages
  are consumed only at the start of a pass on the owning CPU.
- One **quantum** is 10 ms. It governs both bands and every pass-based bound
  below.
- A CPU **publishes its load** — its count of runnable tasks — at each pass.
- The **frontier** is the maximum virtual runtime any CPU has dispatched.

## 2. Invariants

1. **A task exists in exactly one place.** At any instant a task is in exactly
   one of: one CPU's current slot, one run queue, one wait queue, or one
   in-flight cross-CPU message. Two CPUs never act on the same task.
2. **A CPU's scheduler state is exclusively its own.** No other CPU reads or
   writes it; every cross-CPU effect is a message consumed by the owning CPU.
3. **Scheduling completes before the switch.** A pass finishes every decision
   before the context switch; nothing scheduler-related runs when a switch
   resumes.
4. **No wake is lost.** Every blocking site uses the same two-phase protocol:
   the waiter registers, then commits. A wake or a kill arriving between the
   two phases prevents the park. There is no per-source wake mechanism.
5. **A sleeping CPU never holds a ready task.** A CPU enters its sleep state
   only with an empty run queue. A wake sent while the target CPU is entering
   sleep takes effect: the CPU observes it either before sleep entry
   completes or by interrupt immediately after; it does not sleep through it.
6. **A deadline is armed before its CPU can sleep.** A pass cannot end with a
   pending deadline and an unarmed timer.
7. **A killed task is never migrated, and is dispatched exactly as far as its
   own unwind.** The kill takes effect wherever the task is: a parked task is
   made runnable so it can observe the cancel, a ready or running one keeps its
   stack and dies at the first safe point that can end it — the return to Ring
   3, or the exit its own unwind reaches. It is never dispatched into
   *userland* again. Release completes when the unwind does, within one pass of
   the CPU holding the task plus that unwind, after at most one message hop per
   migration in flight, and never waits on a timer.

   **Amended 2026-08-16 by `specs/completion-architecture-spec.md` §7.2, and
   the previous form is quoted because it was load-bearing**: "never dispatched
   again" was what let a killed task be reaped where it lay. This kernel does
   not unwind, so the reap discarded every guard on that task's kernel stack —
   survivable only while the one thing on it was a spinlock guard, and not
   survivable at all once a sleep lock can be held across a park. The property
   that replaces it is the one the reap was standing in for: nothing a killed
   task holds outlives it, because the task itself gives it back.
8. **A cross-CPU message is never dropped.** There is no message capacity to
   exhaust and no overflow.
9. **Userland cannot stall the scheduler.** A wake storm costs the sender its
   own time only.
10. **A wake is not proof of a condition.** Invariant 4's dual, and the
    blocking site's half of it: a task is woken *by name* as well as by queue,
    buckets are shared, and a deadline fires on the task's own CPU, so a park
    that returned proves only that the task runs again. Every blocking site
    re-checks its own condition after every wake and re-parks until it holds or
    its deadline passes. The scheduler never decides what a wake meant, and a
    site that reads the return as the answer is asserting a fact nothing
    established.

## 3. Bands

Two priority bands: **real-time** and **normal**. A ready real-time task
always preempts the normal band. Real-time tasks run FIFO within the band and
round-robin on the quantum. Entering the real-time band requires the RT right
(capability endowment spec).

A real-time writer signalling a pipe lends its band to the blocked reader
(priority inheritance). The lend begins at the signal, survives queue time to
the reader's first dispatch, and ends at the earlier of the reader's next
block or one quantum of running time under the lend.

## 4. Fairness

The normal band is fair per process: a process's share is charged for every
nanosecond any of its threads runs. Ordering is by virtual runtime with
stored lag: a share leaving the runnable set keeps its lag, clamped to
±50 ms, and re-derives it against the frontier on wake.

Equal-virtual-runtime ties break by monotonic insertion order, so a
re-inserted thread lands behind its equal siblings.

Accounting is conserved: every nanosecond of a task's existence is attributed
to run-queue wait, blocked time, or CPU time, and the sum equals elapsed
time.

## 5. Placement

- A woken normal task runs on the CPU that woke it, unconditionally.
  Preemption is decided at the next pass, never at placement.
- A woken real-time task moves only when the waking CPU is itself running
  real-time work and a sleeping peer exists; the move targets that one CPU
  with a directed interrupt. Real-time wake latency is bounded by interrupt
  delivery plus one pass.
- A spawned task is placed on the CPU with the lowest published load.
- An idle CPU with an empty run queue requests work from the CPU with the
  highest published load, then sleeps; the victim answers by its next pass,
  and the answer's arrival wakes the requester. The two-hop latency is
  bounded by one quantum.

The worst accepted latency for a normal wake to a busy CPU is one quantum.

## 6. Failure semantics

Wait queues are FIFO: a wake claims the longest-waiting waiter.

| Race or failure | Behavior |
|---|---|
| Wake races the block commit | The commit refuses to park; no switch happens |
| Wake races a timeout | Exactly one claims the waiter; the wake falls through to the next waiter |
| Wake races a kill | Delivered in order to one owner; the loser is a no-op |
| Normal wake to a busy CPU | Run at the target's next pass, within one quantum, with no interrupt sent |
| Real-time wake to a busy CPU | Directed interrupt; pass at interrupt exit |
| Task killed while in transit between CPUs | The receiving CPU adopts it and dispatches it so it can unwind; the retire chase's `Urgency::Preempt` is what makes that prompt, rather than the destination's next voluntary pass |
| A second concurrent release of one task | Kernel bug; panic |
| Panic inside a pass | The CPU stops with a single report; re-entry into the panic path is detected and does not recurse |
