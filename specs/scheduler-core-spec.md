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
   *userland* again. Release completes when the unwind does, after at most one
   message hop per migration in flight, and within a bound this invariant
   states rather than assumes.

   **Amended 2026-08-16 by `specs/completion-architecture-spec.md` §7.2, and
   the previous form is quoted because it was load-bearing**: "never dispatched
   again" was what let a killed task be reaped where it lay. This kernel does
   not unwind, so the reap discarded every guard on that task's kernel stack —
   survivable only while the one thing on it was a spinlock guard, and not
   survivable at all once a sleep lock can be held across a park. The property
   that replaces it is the one the reap was standing in for: nothing a killed
   task holds outlives it, because the task itself gives it back.

   **Amended again the same day, and this half is a correction rather than a
   replacement.** The release clause used to end "within one pass of the CPU
   holding the task plus that unwind ... and never waits on a timer". Both
   halves were false when they were written. Release needs at least two passes
   after `Dead` is published, because a pass cannot free the stack it is
   standing on; and a queued victim waits out the running task's quantum before
   it is dispatched at all, which is a wait on the programmed hardware timer
   that expiry is. The bound below is what the sentence should have said.

   **"Never dispatched into userland again" is enforced at the last exit
   boundary, and its residual is one interrupt wide.** Every return to Ring 3
   that can carry a task — syscall, exception, device interrupt, TLB shootdown,
   the timer tick and the thread-start trampoline — ends in
   `kernel_exit_to_user_check`, which reads the kill bit *after* its reschedule
   loop has settled and immediately before the return, with interrupts off. The
   bit is set by a remote CPU's plain atomic and can therefore be raised in the
   instant between that read and the `iretq`; the retire sets the bit first,
   with a locked read-modify-write in `claim_retire`, and only then posts the
   message and issues its `Urgency::Preempt` kick — so that kick cannot be
   consumed by a target that has not yet seen the bit, the thread takes the
   interrupt in Ring 3, and it comes straight back through the same boundary.
   **The bound is one interrupt delivery, not one quantum and not one unbounded
   Ring 3 loop**, and it rests on that order: a kick issued *before* the bit
   could be consumed in Ring 0 with the bit still invisible, and the victim
   would then run in Ring 3 until an unrelated tick.
   `toyos-sched/src/retire.rs`'s module header states the same order from the
   other side.

   **The one exception is vector 2**, and the enumeration says so rather than
   saying "every". The NMI stub is a `Ring0Entry`: no preempt-count bump and no
   exit-to-user check, and it `iretq`s straight back to whatever ring it
   interrupted, by its own module header's decision. An NMI taken in Ring 3
   therefore returns to Ring 3 without the check. It is diagnostic-only, it is
   raised by a debug-dump path, and it does not consume the retire's pending
   kick — so the residual above is unchanged by it. What would not be true is
   the word "every".

   Both weaker forms of this bound were live on this branch before 2026-08-16:
   the check ran once *above* the reschedule loop, which gives the CPU away with
   interrupts on; and the Ring 3 timer stub — which is where `apic::kick_cpu`'s
   IPI lands — did not run the epilogue at all, so a thread killed in userland
   was preempted into the dying list, picked straight back off it and returned
   to Ring 3, once per tick, for as long as it cared to loop.

   **Release completes within the retirer's own tripwire, and both the bound
   and its residual are written down.** `kernel/src/scheduler.rs`'s `GIVE_UP`
   carries the derivation: the pass prologues on xHCI's 2 s deadline, two
   quanta, and an unwind a saturated real-time band defers (§3), times one plus
   the corpses queued ahead of this one. **Two of those terms are superseded
   rather than restated here**, and `specs/scheduling-reservations-spec.md` §8
   is where each is answered: the prologue count named at the site is an
   undercount — the measured number of passes inside one corpse's chain is
   twenty, not four, which no constant absorbs — and the real-time factor is a
   deferral bounded per corpse rather than per CPU, so it is not a worst case
   in either direction. **The remaining qualification is that the queue factor
   is workload-shaped and the tripwire is a constant**: with 8.02 s of fixed
   terms and 110 ms per further corpse the sum first reaches the 10 s constant
   at eighteen concurrent unwinds on one CPU, and the 990 ms of margin buys nine
   of them — two quantities that were previously conflated into one. It is filed
   as `specs/issues/kernel/retire-tripwire-is-not-queue-shaped.md` and is not
   claimed away here. An invariant states a derived bound or it is not an
   invariant; this one states the bound and the edge it does not reach. Its
   dominant term is a second filed defect
   (`specs/issues/kernel/scheduler-pass-blocks-in-xhci.md`) and not a property
   of this invariant.

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

**A killed task unwinding its own stack (invariant 7) is normal-band work, and
the real-time band's precedence over it is bounded.** It is dispatched ahead of
the *fair* queue — a retirer is blocked on the resources it is giving back, so
it is not competing for a share of the CPU — and behind every ready real-time
task, which is waiting on nothing it holds. Its unwind is served
first-in-first-out against the other unwinds on its CPU.

**"Normal-band work" is about what the task is doing, not about a right it
holds.** `RtState::release` ends an inherited lend and leaves the *permanent*
flag alone, so a thread that entered the real-time band and was then killed
still answers `is_rt()`. That is correct — nothing revokes the right — but it is
not the question the pick and the preemption ask, and asking it let such a
corpse keep its CPU for a whole quantum against a ready real-time sibling while
the pick was gating its dying list on `rq.has_rt()` regardless: two halves of
this rule disagreeing about one task. `RunningTask::serves_rt_band` is the one
question both halves ask now, and the answer for a dying task is no.

**The qualification, which is derived and not a hedge**: once the head of a
CPU's dying list has waited `toyos_sched::cpu::DYING_AGE_NS` = one quantum, the
next pick dispatches it ahead of the real-time band for one `DYING_CHUNK_NS` =
1 ms, and the band resumes. The stamp restarts, so the next such chunk is a full
age window away. A real-time task therefore gives up at most 1 ms per 10 ms to a
corpse, which is the whole of what invariant I4's bound grows by; and an unwind
under a real-time band that never empties is delivered at one chunk per
11 ms, which is the factor `retire_task`'s tripwire carries.

**Both absolutes were tried and both are wrong**, which is why the sentence
above has exactly one qualification and not none:

- The dying list served *before* the real-time band starves a ready real-time
  task for the whole of an unwind, quantum after quantum, because the
  preemption returns the corpse to the list and the next pick hands it straight
  back. That contradicts the paragraph above it.
- The dying list served strictly *after* the band starves the corpse for ever
  under one thread that holds the RT right and never parks. No sibling CPU can
  rescue it — a killed task is never migrated (invariant 7), and a steal answers
  from the fair band only — so `Hw::release` is never called and the retirer's
  tripwire panics the kernel. `Rights::RT` is capability-gated and `SYS_RT_ENTER`
  has no revocation, so that is a kernel panic reachable from a legal workload,
  which is the one thing this kernel does not do.

`toyos-sched`'s `a_killed_task_does_not_starve_a_ready_rt_task` and
`a_corpse_is_not_starved_for_ever_by_a_spinning_rt_task` are the two gates, one
per direction, and the simulator carries the same pair as invariant I4 and the
negative gate `old_rt_starved_the_corpse`.

A real-time writer signalling a pipe lends its band to the blocked reader
(priority inheritance). The lend begins at the signal, survives queue time to
the reader's first dispatch, and ends at the earlier of the reader's next
block or one quantum of running time under the lend.

**Amended by `specs/scheduling-reservations-spec.md` §1.8, and applied at that
document's R4; the shipped form above is what the tree still implements.** The
paragraph's defect is that it lends a *band* — an unbounded precedence over
everything below it — and the reservation model has no band to lend. Its
replacement is the **urgency mark**: the wake marks the woken reader, and a
marked thread is dispatched ahead of unmarked threads **inside its own
scheduling class**, for a bounded window, spending its own class's budget and
never the waker's. The mark ends at the first of that window, the reader's next
block, or the wait that raised it ending; it moves no budget, so a reader that
runs long delays only its own class, and a waker's reservation is not something
its wakees can drain. The quantum of running time above has no counterpart,
because a quantum was never the quantity being lent.

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
