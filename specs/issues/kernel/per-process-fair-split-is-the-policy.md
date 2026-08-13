---
status: none
kind: rejected
opened: 2026-08-01
---

# The scheduler's *per-process* fair split degrades as the machine widens — settled: it is the policy

Worst service spread against the derived bound, in ms, from
`measure fairness_storm:<cpus> 500`:

| CPUs | 1 | 2 | 3 | 4 | 6 | 8 | 12 | 16 | 24 | 32 |
|---|---|---|---|---|---|---|---|---|---|---|
| worst | 30 | 84 | 125 | **198** | **324** | **418** | **634** | 720 | 1056 | 1386 |
| bound | 60 | 108 | 156 | 204 | 300 | 396 | 588 | 780 | 1164 | 1548 |

**Per-process only, and that is a real bound on the defect.** The *per-thread* split does
not degrade with width: measured 10 ms at 1 CPU to 50 ms at 32, against a 60 ms derived
bound — inside its bound at every width — over the same runs where I5 went 30 → 1386. So
threads of a process are shared out fairly among themselves at any machine size; it is the
split *between processes* that widens. The fix has a smaller target than "fairness degrades"
implies.

**Both questions the earlier filing left open are now measured, not argued.**

**Offset, not drift.** Holding the seed count and scaling the storm's per-thread
work: one CPU stays at 30 ms at every window length, while eight go 362 → 602 →
548 ms as the window doubles twice. It saturates rather than accumulating.

**Policy, not model.** Everything deciding who runs next is the shipped core —
`RunQueue`'s insertion-time keys, `FairShare`'s one vruntime pot per process,
`CpuSched::pick`, `answer_steal_requests`' surplus rule. The simulator mocks
time, timer, IPI, halt and switch: the parts that decide *when*, not *who*.

**The mechanism, which is why this is a design consequence and not an
implementation bug.** Every running thread of a process charges one pot, so the
pot advances at the process's *aggregate* rate while each queued thread's key
stays frozen at its insertion. One dispatch of staleness therefore buys more
wall-clock service the more of that process runs at once. That is why it scales
with width, and why careful coding cannot close it — the fix is a policy change.

**Caveat, and it is load-bearing.** These are worst-of-N over adversarially
chosen interleavings, seeded and PCT — not the split hardware would show on an
average schedule. **The mechanism and the scaling are the policy's; the magnitude
is a worst case.** Do not quote these numbers as expected behaviour.

**Connected to §9.2's tie-break, and that is why this is hard.** Threads of a
process sharing one vruntime is *why* the insertion sequence exists
(`specs/scheduler-core-spec.md` §9.2, `queue.rs:18-22`). The degradation here and
sibling starvation are two faces of one decision: **per-process accounting with
per-thread queueing.** Anything that fixes one has to answer for the other.

**But only the per-process face degrades, and that is now measured.** Simulator
invariant I13 measures service per *thread* inside a share over the same
contention windows, narrowed to intervals where every CPU carries the same
number of each member's runnable threads (otherwise the number is placement, not
ordering). From `measure fairness_storm:<cpus>`, against a derived bound of
60 ms at every width — `(rivals + 1) × (QUANTUM + max KernelSection +
2 × RUN_CHUNK)`, five dispatches of one run queue's fair band, with **no lag
term** because a share holds one vruntime and one lag for all its threads:

| CPUs | 1 | 2 | 3 | 4 | 6 | 8 | 12 | 16 | 24 | 32 |
|---|---|---|---|---|---|---|---|---|---|---|
| I13 worst | 10 | 30 | 28 | 28 | 31 | 32 | 35 | 37 | 42 | 50 |
| I5 worst | 30 | 102 | 125 | 198 | 324 | 418 | 634 | 612 | 1046 | 1386 |

Flat where the per-process split runs away. **And the tie-break is not what
keeps it flat** — the pot is charged for every nanosecond any thread of the
share runs, so a re-inserted thread already carries a key strictly above every
sibling queued before it and the band serves them in insertion order whatever
the tie-break is. `(vruntime, TaskKey)` ported literally
(`scenarios::fair_identity_tiebreak`) is invisible to I13, which is why the
negative gate had to be the stronger `fair_identity_within_share`. **The
consequence for the fix**: a redesign replacing per-thread queue keys with an
ordered map of shares each holding a FIFO of its ready threads takes the
ordering job *away* from the pot and hands it to that FIFO, so this face stops
being benign the moment the fix lands. I13 is the gate that says so; it is green
today and its own gate is red on the broken shape, on I13 alone — I5 reports a
perfectly even split while two of three sibling threads never run.

**Entry criteria for the per-share-FIFO redesign.** I5 and I13 together are
close to sufficient and are not sufficient. Three gaps, all prerequisites rather
than follow-ups, and the first is *the* one — the other two are conditions on
trusting the answer, this one is a hole where the answer would be.

1. **The redesign's most novel path has no coverage in the workload class that
   exercises it.** Where a woken thread lands in its share's order falls out of
   the pot today; after the redesign it is decided by the FIFO push, which *is*
   the new code. Nothing measures it. A block drops a thread from I13's measured
   set, so I13's reach inverts exactly against the workloads that would exercise
   it — 96–99% on the fairness storms, where nothing blocks, against
   `crash_md_exit_race` 37%, `rt_wake_latency` 29%, `fork_storm` 9%,
   `futex_storm` 5% and `audio_pipeline` **0%**. **I13 would stay green straight
   through a redesign that got the wake path's ordering wrong**, and it is the
   check that nominally guards fairness. A wake-heavy workload with windows long
   enough to measure does not exist and has to be built first.
2. **I13's reach is a silent casualty of the change it guards.** Its window
   closes when a member's threads stop being evenly spread over the CPUs, and
   the redesign must reimplement `pop_surplus`, which feeds
   `answer_steal_requests` and can therefore change placement — so a redesign
   that disturbs placement makes I13 measure *less* rather than fail, with the
   sweep still printing `clean`. Instrumented rather than left as vigilance:
   `SweepResult::thread_coverage_pct` publishes the fraction of executed time
   I13 had a comparison open for, `invariant_i13_is_measured_and_holds` gates on
   it against 96% / 69% / 99%, and forcing the balance condition false takes it
   to 0% and reds the test. **A/B that number across the redesign; a collapse is
   as loud as a violation.** Named as the third gate-failure shape in
   `specs/spec-staleness-sweep.md`, with the evidence in
   `specs/assessments/metal-track-history.md`.
3. **The margin at 32 CPUs is 1.2× and trending up** — 10 ms at one CPU to
   50 ms at 32 against a 60 ms bound — with nothing measured above 32, while
   spec §11 Stage 9 gates on 1–128. Measure 64 and 128 first, or a red at high
   width cannot be attributed to the redesign rather than to the width.
   Compounded by the reach falling with width for an unrelated reason — 55% at
   four CPUs, 45% at eight, because threads exit at slightly different moments
   and unbalance a wide machine sooner. **At the widths Stage 9 targets, I13
   certifies less than half the run**, which is a limit on the invariant and not
   a defect in it.

Found only because I5 measures *service* — nanoseconds actually delivered — rather
than checking vruntime bookkeeping against itself, which would have been true by
construction. The dead-gate lesson from the other side: the first question about a
gate is not whether it passes, but whether it measures the quantity you care
about (`specs/spec-staleness-sweep.md`).
