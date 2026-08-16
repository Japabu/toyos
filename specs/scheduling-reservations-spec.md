# Scheduling reservations

CPU time is a reservation. Every schedulable entity on a CPU holds a
`(budget, period)` pair, admission arithmetic refuses a set of reservations that
does not fit, and a runnable entity is never served below the reservation it
holds. There is no band whose precedence is unbounded, in either direction.

This document is normative for the reservation layer and amends
`scheduler-core-spec.md` where the two meet. It exists because the property it
states was previously derived pairwise — a bound on how long real-time work
waits for a corpse, a second bound on how long a corpse waits for real-time
work, a stretch factor connecting them, and a tripwire spending that factor —
and each derivation was correct for one shape of workload and false for another.
Three attempts moved the failure rather than removing it: real-time work ahead
of unwind work starves the unwind, unwind work ahead of real-time work starves
real-time, and an aged grant between them starves whichever side the workload
happens to put more of on one CPU. One invariant replaces all of it.

---

## 1. The model

### 1.1 A reservation

A **reservation** is a pair of wall-clock nanosecond quantities, `budget` and
`period`, with `0 < budget ≤ period`. Its **utilization** is `budget / period`,
carried in the kernel as **permille of one CPU** so that the admission
arithmetic is integer and exact: a reservation's utilization is
`ceil(1000 × budget / period)`, and rounding is always upward, against the
holder.

A reservation is held **on one CPU**, and its utilization is a fraction of *that
CPU's* capacity. It is not a share of the machine and it does not follow a
thread across a migration by itself (§1.10).

### 1.2 The entities that hold one

Exactly three kinds of entity hold a reservation, and every nanosecond a CPU
executes is charged to one of them:

- **A real-time client.** One thread that has entered the real-time band. Its
  reservation comes from its process's endowment (§7).
- **The fair class.** One entity per CPU, holding the reservation the other two
  kinds leave. Every fair-band thread on that CPU runs inside it, and the fair
  class's internal ordering is a replaceable policy (§2).
- **The dying server.** One per CPU, kernel-owned, holding the reservation that
  serves killed threads unwinding their own stacks (§1.7).

A CPU's idle state is not an entity: an idle CPU has no runnable entity, and
unspent budget on an idle CPU is not owed to anybody.

### 1.3 Admission

The **admission test** for one CPU is:

> the sum of the utilizations of the real-time reservations placed on that CPU,
> plus the dying server's utilization, may not exceed the machine's real-time
> ceiling.

**The test is per CPU against that CPU's own capacity, and capacity is not
assumed equal across the machine.** A CPU's capacity is 1000 permille *of
itself*; a machine whose cores differ in throughput — big and little clusters,
performance and efficiency cores — is a machine where the same budget is a
different fraction on two different CPUs, and admission is run against the
capacity of the CPU a reservation is actually placed on. A reservation is never
an absolute quantity of work that assumes every core is the same one.

The three policy numbers, per CPU, as fractions of that CPU's capacity:

| quantity | value |
|---|---|
| capacity | 1000 permille |
| the fair class's guaranteed floor | 500 permille |
| the dying server's reservation | 100 permille — 1 ms every 10 ms |
| therefore the real-time ceiling | 400 permille |

The fair class's reservation is whatever admission leaves — capacity minus the
admitted real-time reservations minus the dying server — and it is never below
the floor, because that is what the ceiling is derived from. A real-time client
that does not exist gives its share to the fair class; a fair class with nothing
runnable gives its share to whoever is runnable. The floor is a guarantee, not a
cap, and the same is true of every other reservation in the table: a reservation
is the least an entity gets, never the most it may have.

**Overcommit is refused where the reservation is created, by name**, and never
observed later as a latency:

- For the two kernel-owned reservations the check is static: the dying server's
  100 permille and the fair floor's 500 are constants, their sum is below
  capacity, and the constant that would break it does not compile.
- For a userland real-time grant the check runs at endowment, before the program
  is started (§7.2), and again at the build that produced the manifest (§7.4).

There is no runtime path that creates a reservation, so there is no runtime path
that can overcommit one.

### 1.4 Dispatch

Within a CPU, the entity dispatched is **the runnable entity with the earliest
deadline among those with budget remaining**. An entity's deadline is the end of
its current period. A tie is broken deterministically by kind — real-time
client, then dying server, then fair class — so that a replay of the same
choices dispatches the same entity.

An entity with no budget remaining is not a candidate; it competes inside the
fair class instead (§1.5). An entity with no runnable work is not a candidate.

Which *thread* runs, once an entity is chosen, is that entity's own question:
for a real-time client the entity is the thread; for the dying server it is the
head of the CPU's unwind queue; for the fair class it is §2's policy.

### 1.5 Exhaustion, which is a degraded answer and never a silence

An entity that spends its whole budget inside one period is **demoted into the
fair class for the remainder of that period**. It does not stop, it is not
requeued at the back of anything, and it is not preempted merely for having been
demoted: it becomes ordinary fair-band work, charged to the fair class's share
like every other fair thread, and it competes there under §2's policy.

**Demotion is counted and named.** Every entity carries a count of the periods
in which it exhausted its budget, and that count is reported by the same
instruments that report the rest of a CPU's scheduler state. A machine whose
audio client is being demoted every period is a machine whose reservation is
sized wrong, and that is a number a reader can find rather than a jitter a
listener can hear.

Demotion is the only behaviour at exhaustion. There is no throttle that stops an
entity, because a stopped unwind is a resource nobody gets back and a stopped
audio mix is a dropout; and there is no silent extension, because a reservation
that quietly grows is not one.

### 1.6 Replenishment

Budget is replenished **at the period boundary and nowhere else**: when an
entity's deadline passes, its budget is refilled to `budget` and its deadline
advances by `period`.

- **Unspent budget does not carry over.** A carried-forward budget lets an idle
  entity accumulate a claim that the admission arithmetic never priced, which
  would make the sum in §1.3 false of the instant it matters.
- **A wake inside a period does not replenish.** An entity that becomes runnable
  partway through its period resumes on whatever budget that period has left.
  Waking is not an event that creates CPU time.
- **An entity idle across whole periods rejoins at the current boundary**: its
  deadline is advanced to the first boundary at or after the wake, and its
  budget is full there. It never carries a stale deadline that would let it
  outrank everything on the CPU on the strength of having been asleep.

A client whose reservation period matches the rate it is woken at therefore gets
exactly one full budget per wake, which is what §4 sizes the audio reservation
to be.

### 1.7 The dying server

A killed thread unwinding its own stack is served by the **dying server**, an
ordinary reservation client that happens to be owned by the kernel rather than
by a process.

- Its runnable set is the CPU's queue of killed threads, served
  first-in-first-out. A killed thread is never migrated, so the queue it stands
  in is the queue of the CPU that owned it.
- It is dispatched, preempted, demoted and replenished by §§1.4–1.6, with no
  rule of its own. It has no age, no stamp, no grant and no chunk.
- Its reservation is a **floor, not a cap**: on a CPU with nothing else runnable
  the dying server exhausts its budget, is demoted into the fair class, and
  keeps running there — so an unwind on an idle CPU is not slowed down by the
  existence of a reservation for it. On a CPU whose real-time clients are all
  spending their full budgets, it still receives 1 ms of every 10 ms, because
  admission guaranteed there was room for it.

The two failure shapes that the previous designs alternated between are both
unrepresentable: real-time work cannot hold the dying server below its
reservation, because the reservation was admitted against real-time's own; and
the dying server cannot hold real-time work below *its* reservation, because
budget is the only thing it can spend and it has 100 permille of it.

### 1.8 Donation replaces the lent window

Priority inheritance is **budget donation**. A thread blocked on a resource
another thread holds donates its reservation — the remaining budget of the
current period, and the deadline — to the holder. The holder runs against the
donor's deadline and spends the donor's budget.

- **Donation creates no reservation**, so the admission sum in §1.3 is
  unchanged by it. This is the whole reason the mechanism is a donation and not
  a promotion: a promotion mints precedence that admission never priced, which
  is the mechanism this document exists to abolish.
- **A donation ends** when the holder blocks, when the donated budget is spent,
  when the holder is killed, or when the donor stops waiting — whichever is
  first. It does not survive the holder's next park.
- **A donation does not cross a CPU.** Budget is per-CPU, so the wake that
  donates places the holder on the donor's CPU. Placing a thread there adds no
  reservation and therefore needs no admission check.
- A donated budget is charged to the donor's reservation, not to the holder's
  fair share. The donor asked for the work; the donor pays for it.

### 1.9 The invariant

> **No runnable entity is served below its reservation.** Over any window of one
> period during which an entity is continuously runnable, that entity receives
> at least its budget of CPU time, measured on the wall clock.

This is the whole of the liveness claim. It replaces every pairwise derivation
between the bands, and every constant that existed to express one side of such a
derivation. Starvation is not bounded by it — starvation is unrepresentable
under it, because an entity that could be starved is an entity whose reservation
was admitted and therefore is not.

The wall clock is the measurement, deliberately: it is the clock the retirer's
own guard reads, and a model that measures liveness on a clock the kernel cannot
read is a model that cannot see the failure the kernel dies of.

### 1.10 Reservations and placement

A thread holding a real-time reservation is **pinned to the CPU its reservation
was admitted on**. A move requires releasing the reservation on the source CPU
and admitting it on the destination, and a move whose admission fails does not
happen — the thread stays where it is rather than moving and losing its
guarantee.

The fair class and the dying server are per-CPU by construction and do not move.
Fair-band threads move freely: they are inside the destination CPU's fair class
when they arrive, and the fair class's reservation is not a function of how many
threads are in it.

---

## 2. The intra-fair policy seam

The reservation layer is mechanism. It decides how much CPU each entity is
guaranteed and when each is dispatched; it does not decide which fair-band
thread runs when the fair class is dispatched. That decision is a **policy
behind a seam**, and the seam is deliberate.

- **The current policy stands unchanged by this document.** The fair band is
  ordered per process by virtual runtime with stored lag against the frontier,
  ties broken by monotonic insertion order — `scheduler-core-spec.md` §4, and it
  is the seam's current occupant, not a property of the reservation layer.
- **The seam's contract.** An intra-fair policy may reorder fair-band threads
  freely, and may keep whatever per-thread or per-process state it needs. It may
  not read or write any reservation, may not change how much CPU the fair class
  as a whole receives, and may not create an entity. Anything it does is
  therefore invisible to §1.9: no intra-fair policy can starve a real-time
  client or the dying server, and none can be starved by them, because the fair
  class's floor was admitted before the policy ran.
- **A replacement is gated by the simulator's invariants**, not by review: the
  per-process fairness invariant, the identity-within-share invariant, and
  §6.1's reservation invariant all apply to a new policy exactly as they apply
  to the current one, and the negative gates that hold the current policy's
  shape are not weakened to admit a new one.
- **The intended future occupant is EEVDF-shaped virtual-deadline scheduling**,
  the school that Linux's default fair scheduler and its interactivity-tuned
  derivatives belong to: an interactive thread betrays itself by sleeping long
  and running in short bursts, so the fair band is ordered by lag and virtual
  deadline, and a just-woken thread earns the *next* slice without earning any
  more total share. That is what buys desktop responsiveness without a heuristic
  priority boost, and the reservation layer must simply not preclude it. It is a
  queued track and not this document's work.

In the code the seam is the run queue's fair-band ordering and the fair-share
arithmetic beside it — two files, which is what makes "replaceable" a fact
rather than an aspiration: `toyos-sched/src/queue.rs` and
`toyos-sched/src/fair.rs`.

**A second, different seam is placement**, and it is named here only so that
this model does not close it. Choosing *which* CPU a thread runs on — matching a
thread's character to a class of core, and spending energy as a scheduling input
rather than as a consequence — is a queued track on machines whose cores are not
alike. The reservation model must merely not preclude it, and §1.3's rule that
capacity is per CPU is what keeps that true: a placement policy that moves a
thread between unlike cores re-runs admission on the destination, and the answer
it gets is arithmetic rather than an assumption. Nothing about that policy is
designed here.

---

## 3. What this deletes

Each deletion is named with the file that holds it. Nothing here is deprecated,
kept behind a flag, or left as a fallback.

### 3.1 The aged grant and everything that expressed it

`toyos-sched/src/cpu.rs`

- `DYING_AGE_NS` and `DYING_CHUNK_NS`, and both derivations in their docs.
- `CpuSched::aged_grant`, the field and every read of it.
- The per-corpse stamp: `Corpse::since`, the `since` restamp in `keep_dying`,
  and the aging test on `dying.front()` in `pick`.
- `pick`'s `!rq.has_rt() || aged` gate, and the whole ordering rule it
  expressed. Under §1.4 the pick asks which entity has the earliest deadline
  with budget; it does not ask which band is occupied.
- `preempt_if_due`'s `!aged_grant` term in the real-time preemption test.
- The dying list's field doc paragraph asserting that the pick serves the run
  queue first whenever the real-time band is occupied, and that a dying task is
  preempted for real-time work exactly as any other fair-band task is. Both
  halves stopped being true when aging landed; both are deleted rather than
  re-qualified, because under §1 neither question is asked.
- The `rt_outranks_every_corpse` escape hatch, replaced by §6.3's stronger pair.

### 3.2 The stretch factor and the terms that spend it

`toyos-sched/sim/src/invariants.rs`

- `rt_deferral_stretch`, the 11× factor, and its use as a multiplier in
  `retire_latency_bound`.
- The `DYING_CHUNK_NS` term of `rt_latency_bound`. The real-time latency bound
  is re-derived in §5.3, not re-priced.
- `check_retires`'s statement of the retire bound in aged-grant terms.

`toyos-sched/sim/src/vm.rs`

- The paragraphs of `Killed`'s doc that argue the bound from bounded deferral.
- `Killed::seen_on`, which has been dead since I14 moved to the wall clock, is
  deleted independently of this design and ahead of it.

`toyos-sched/sim/src/workload.rs`

- The workload doc's restatement of the age-then-chunk rule.

### 3.3 The pairwise liveness derivations

`specs/scheduler-core-spec.md`

- §3's qualification paragraph — the age window, the one-chunk grant, the
  restarting stamp, the "at most 1 ms per 10 ms", and the "one chunk per 11 ms"
  delivery rate — and the two "both absolutes were tried" bullets under it.
  §5.1 states what replaces them.
- Invariant 7's release clause insofar as it prices the unwind through the
  stretch factor. §5.2 states what replaces it.

`kernel/src/scheduler.rs`

- `GIVE_UP`'s elevenfold-stretch term and its `peers = 8` pricing. §8 states
  what replaces them, and the mechanical pass that precedes this design corrects
  the provenance of `peers` in place rather than re-deriving a doomed form.

`specs/completion-architecture-spec.md`

- §7.3's outline of the derivation, insofar as it restates the four pass
  prologues, the 11× factor, and `peers = 8`. §5.6 records the reconciliation.

### 3.4 The three findings this design answers, at the statements that were false

The adversarial pass that preceded this document confirmed three unsound
statements about bounded deferral, and each is answered here rather than
patched:

- **Bounded deferral was bounded per corpse and not per CPU.** `k` corpses on
  one CPU took `k` consecutive grants, so real-time work waited `k` milliseconds
  per age window and the stated "at most one chunk" was a `k = 1` argument;
  at `k ≥ 11` the rotation closed on itself and the real-time band was starved
  without bound. Under §1 there is no per-corpse grant to multiply: the dying
  server is one entity with one reservation however many corpses it holds, and
  the real-time clients' reservations are what the queue depth cannot touch.
- **The stretch factor was not a bound in the other direction.** A real-time
  band that briefly emptied dispatched the corpse instantly with a full
  unprotected quantum and no aged grant, and the next real-time wake restamped
  it — so the corpse's accumulated age was thrown away on every brief dispatch
  and its service rate had no lower bound at all. Under §1.6 there is nothing to
  restamp: budget is replenished on a period boundary the workload cannot move,
  and a brief dispatch spends budget rather than resetting a claim to it.
- **The real-time latency term was a `k = 1` term.** The simulator's real-time
  bound carried a single chunk. §5.3 re-derives it against the reservation
  instead, where the quantity that bounds real-time latency is the sum of the
  other entities' budgets and not the number of corpses.

---

## 4. The audio contract

### 4.1 soundd's reservation

`soundd` holds a real-time reservation of **580 µs every 2.902494 ms** —
19.98 % of one CPU, admitted as 200 permille.

The period is the device period exactly: 128 frames of stereo 16-bit audio at
44 100 Hz, which is the rate at which the device consumes a buffer and therefore
the rate at which the mix thread is woken. A reservation period equal to the
wake period is what makes §1.6's replenishment rule give the mix thread exactly
one full budget per wake, and it is what bounds its wake-to-run latency by one
device period rather than by a quantum.

**The budget is an estimate and is declared as one.** The only recorded figure
for soundd's CPU cost is roughly 7 % of a core with no clients, which is 203 µs
of a 2.902494 ms period; the budget is that figure with the headroom a mixing,
resampling, multi-client server needs over an idle one. It is not a
measurement of the mix path, and the chunk that installs it re-measures soundd's
per-period CPU time on the four gate-A configurations and replaces the number
with what it reads. A reservation sized by estimate is a reservation whose
exhaustion counter (§1.5) is the thing to read on the next boot.

The admission arithmetic for the shipped machine, on one CPU: 200 permille for
soundd, 100 for the dying server, 500 for the fair floor — 800 of 1000, with 200
permille spare. The same set is admitted at every CPU count, because the
reservation is placed on one CPU and the rest of the machine is unaffected by
it.

### 4.2 Acceptance is measurement, and never-degrade-audio is enforced by it

The reservation lands only if the audio gate says the audio did not move. The
acceptance criteria, and none of them is optional:

- **Gate A's four configurations** — the tone and the tone-under-load workloads,
  each at one CPU and at eight — measured against the recorded sample in
  `tests/audio-baseline.toml`.
- **The per-run ceilings** in that file: worst-wake-lateness, drains, and
  underruns. A run that breaches one is a red, and the ceilings are a
  catastrophe detector rather than the sensitive instrument.
- **The distributional comparison** in gate A's thorough tier, against the
  recorded per-run samples. This is the instrument with the power: a change that
  moves the wake-lateness distribution is rejected even when every run is inside
  its ceiling.
- **Gap and dropout counts**, which are the audible quantity and are asserted at
  zero.

A reservation that improves the ceilings but moves the distribution the wrong
way is a regression and is not landed. Re-recording the baseline to accommodate
this change is not available: the baseline moves when the machine is measured to
be better, on a quiet host, with the conditions recorded — not when a change
needs it to.

---

## 5. Reconciliation

### 5.1 `scheduler-core-spec.md` §3 — bands

§3 becomes a statement about reservations rather than about precedence, and the
amendment replaces the whole of the qualification paragraph struck in §3.3:

> There are three kinds of schedulable entity — real-time clients, the fair
> class, and the dying server — and each holds a reservation. Dispatch is by
> earliest deadline among entities with budget remaining. A real-time client
> does not preempt the normal band by right; it is dispatched when its deadline
> is the earliest, and it is guaranteed its budget every period because
> admission refused any set of reservations that could not deliver it. Entering
> the real-time band requires a reservation, which is a capability endowment.

The sentence "a ready real-time task always preempts the normal band" does not
survive, and neither does its negation. The two "both absolutes were tried"
bullets are deleted with the qualification they justified: under the reservation
model neither absolute is expressible, so recording that both were tried belongs
to this document's opening and not to the law.

A killed task unwinding its own stack remains **normal-band work in the sense
that mattered** — it is not real-time work and holds no real-time right by
virtue of dying — but the sentence that carried that meaning is replaced by
membership: it is the dying server's work, and the dying server is an entity
with a reservation. The question "does this task serve the real-time band" stops
being asked at the pick, because the pick asks about entities.

### 5.2 Invariant 7 — a killed task is dispatched exactly as far as its own unwind

The invariant's first three paragraphs stand unchanged: the kill takes effect
wherever the task is, the task is never migrated, it is never dispatched into
userland again, and the exit-boundary residual is one interrupt delivery wide
resting on the kill-bit-before-kick order. None of that is a reservation
question and none of it moves.

The **release clause** is amended. Its current form states a bound derived
through the stretch factor and a workload-shaped `peers` term against a
constant. Its reservation form:

> Release completes within a bound this invariant derives from the dying
> server's reservation: the victim's own unwind, plus the unwinds queued ahead
> of it on that CPU, each delivered at the dying server's guaranteed rate, plus
> the message and pass hops at either end. The bound is a function of a declared
> reservation and a queue depth the retirer can read, and not a constant that a
> queue can outgrow.

§8 is the derivation.

### 5.3 Invariant I4 — real-time wake latency

I4's current bound is interrupt delivery, plus the longest preempt-off section,
plus the observation granularity, plus one dying chunk. The fourth term is
deleted (§3.2) and re-derived: what a real-time client waits for is not a
corpse's chunk but **the budget the other entities on its CPU may legitimately
spend before its own deadline arrives**.

> I4's bound is interrupt delivery, plus the longest preempt-off section, plus
> the observation granularity, plus the sum of the budgets of the entities on
> that CPU whose deadlines are earlier than the waking client's.

The last term is a quantity admission bounds by construction: it cannot exceed
capacity minus the client's own reservation, and for a client whose period is
the shortest on its CPU — which is what §4 makes the audio client — it is the
budget of at most one period of each other entity. The bound is therefore
tighter than the one it replaces for the workload that matters, and it is a
bound in every workload rather than in the one-corpse case.

### 5.4 Invariant I14 — a retire reaches release

I14 keeps its clock — the wall clock, for the reason the current design already
records — and keeps its `(1 + peers)` structure, because one CPU runs one unwind
at a time and pretending otherwise prices the machine rather than the protocol.
What changes is the rate: each unwind's wall-clock cost is the victim's own
unwind time divided by the dying server's utilization, plus one period for the
phase the retire arrives in. The stretch factor is deleted and the rate replaces
it.

I14 is not a special case of §1.9; it is a consequence of it, and it stays a
separate check because it measures the composition — the message hop, the queue,
the rate, and the zombie-freeing pass — rather than one entity's service.

### 5.5 The negative gates

The ten negative gates and two controls are law. Each is stated below as
surviving verbatim, re-derived, or replaced by a strictly stronger gate. **None
weakens, and the count does not fall.** Two are added.

| gate | disposition |
|---|---|
| `old_steal_port` | **survives verbatim.** The old steal-and-scan algorithm is a placement and ownership defect; reservations touch neither. |
| `old_commit_before_pass` | **survives verbatim.** The blocking protocol is untouched. |
| `old_preemptible_window` | **survives verbatim.** The registration window's preempt-off requirement is untouched. |
| `old_migrate_kept_the_corpse` | **survives verbatim.** A corpse handed to another CPU is a corpse taken away from the dying server that was admitted to serve it, which is if anything a sharper statement of the same break. |
| `old_rt_starved_the_corpse` | **replaced by a strictly stronger gate.** Its escape hatch stages "the real-time band outranks every corpse", which is one point of the design space this document deletes. Its replacement stages "the dying server is dispatched only when no real-time client is runnable" — the same break expressed against entities — and reds under the reservation invariant rather than under a bound. Stronger because it also reds on the two shapes the old gate could not see: many corpses on one CPU, and a real-time band that briefly empties. |
| `old_park_kept_the_lend` | **re-derived.** The break is unchanged — a park that keeps a lapsed window — but the window is now a donated budget, so the gate stages a park that keeps a donation and the invariant that catches it is the donor's reservation being spent by a thread that no longer holds anything the donor waits for. |
| `fair_share_per_thread` | **survives verbatim.** Per-process fairness is inside the fair class and unaffected. |
| `fair_double_charge` | **survives verbatim.** |
| `fair_identity_within_share` | **survives verbatim**, and gains a second job: it is the gate that holds §2's seam, since an intra-fair policy replacement that regressed thread identity would red here. |
| `overlong_pass` | **survives verbatim.** The pass budget is a property of a pass, not of a band. |
| `old_commit_fused` (control) | **survives verbatim**, and must still come back clean. |
| `fair_identity_tiebreak` (control) | **survives verbatim**, and must still come back clean. |
| **`old_unbounded_rt_precedence`** (new) | Stages the abolished rule: a real-time client is dispatched whenever it is runnable, ignoring budget and deadline. Must red under §6.1 on every seed on which a real-time client can spend more than its reservation. |
| **`old_aged_grant`** (new) | Stages *this document's predecessor* — per-corpse age stamps, a one-chunk grant ahead of the real-time band, and a restamp on every re-entry. Must red under §6.1 on the many-corpse shape and on the briefly-empty-band shape. The design being replaced becomes the gate that proves the replacement measures something. |

The count moves from ten to twelve. A count that fell would be this document
weakening the harness to admit itself, which is the one thing the negative-gate
rule forbids.

### 5.6 `completion-architecture-spec.md`

**§23's rejected list is not contradicted, and this document adds nothing to
it.** Its twelve entries reject a global completion registry, a ring arena, a
second park channel, a spinning sleep lock, sleep-lock poisoning, userspace-only
blocking wrappers, a shootdown-as-completion, interrupt-driven serial transmit,
a single housekeeping thread, ISR-context completion posting, a kernel-side log
daemon, and multishot polls. None is a scheduling-band or precedence rejection,
and none of their reasons rests on the rule this document abolishes. Checked
individually, one interacts and it is strengthened rather than weakened: the
rejection of ISR-context posting rests on measured wake latencies being large
against the ISR-to-drain latency, and a reservation that bounds the audio
client's wake latency by one device period leaves that argument true and its
motive smaller.

**§7.3's outline** is amended by §3.3 and §8: the four pass prologues, the
elevenfold stretch and `peers = 8` are superseded rather than re-derived, and
the section's own instruction — that the derivation lives at the site and is not
restated there — is what the amendment restores.

**§7.2a's amendment stands.** The contradiction it recorded, between a killed
task being dispatched and the law saying it never is, was resolved in the law
and is not reopened here.

**§24's open risk about the audio wake latency to read on the next boot** is
what §4.2 turns into an acceptance criterion. The risk is not closed by this
document — it is closed by the measurement §4.2 requires.

---

## 6. The simulator

### 6.1 One new invariant

> **I15 — a runnable entity is never underserved.** For every entity, over every
> period in which it was continuously runnable, the CPU time it received is at
> least its budget.

It is checked after every step, like the other global walks, and it is measured
on the wall clock. Its violation message names the entity, the period, the
budget owed and the service delivered, so that a red says which side of the
machine lost rather than that a bound was exceeded.

I15 is the only new invariant. I4 and I14 are re-derived (§5.3, §5.4) and stay;
the fairness, ownership, timer, boost-window and accounting invariants are
untouched.

**The liveness half is required too**, under the same rule the other measured
invariants follow: a run in which no entity was ever continuously runnable for a
whole period proves nothing, so the harness reports the fraction of periods I15
actually compared and gates on that number as well as on pass or fail. A change
that closes I15's window is as loud as one that violates it.

### 6.2 The standing scenario matrix

The shapes that broke the three previous attempts become the standing scenario
set, and they are run in both directions: every scenario asserts both that
real-time work got its reservation and that the dying server got its.

| dimension | values |
|---|---|
| corpses on one CPU | 0, 1, 2, 3, 9, 11, 16 |
| real-time load | none; one client inside its reservation; one client that spends its whole budget every period; one client that attempts to exceed it (which §1.5 demotes) |
| real-time band continuity | continuously occupied; briefly empty with the gap at one interrupt delivery, at one execution chunk, and at zero |
| fair load | idle; one thread; a storm |

The three values that are not arbitrary:

- **9 corpses** is what the superseded tripwire priced as its workload term.
- **11 corpses** is where the aged grant's rotation closed on itself, because
  eleven one-millisecond chunks fill an age window plus a chunk. It is the shape
  that must red under `old_aged_grant` and pass under the reservation.
- **A zero-length gap** in the real-time band is the shape under which the aged
  grant's measured service rate had no lower bound at all.

The scenario set is a matrix and not a list: a fix for one direction that breaks
the other is what the pendulum was, and a matrix that is only run in one
direction is how each of the three attempts looked correct when it landed.

---

## 7. Admission plumbing

### 7.1 The manifest

A program's real-time grant is declared in `system.toml` beside the rest of its
endowment. `syscap = ["rt"]` keeps its place and changes meaning: it says the
program **may hold a scheduling reservation**, which is the capability-native
form of the right. It no longer says anything about precedence, because
precedence is no longer a thing a right can confer.

The reservation itself is a new row in the program's table, with both quantities
named in nanoseconds and neither defaulted:

```
[programs.soundd]
serves  = ["soundd"]
devices = ["hda-audio", "virtio-sound"]
syscap  = ["rt"]
reservation = { budget_ns = 580_000, period_ns = 2_902_494 }
```

Rules the parser enforces, each refusing by name:

- A `reservation` row without `rt` in `syscap` is refused: a reservation is what
  the right authorizes, and a program that declares one without the right has a
  manifest bug rather than a smaller endowment.
- `rt` in `syscap` without a `reservation` row is refused: the right with
  nothing to hold is the unbounded band this document abolishes.
- `budget_ns = 0`, `period_ns = 0`, or `budget_ns > period_ns` is refused.
- An unknown key inside `reservation` is refused, exactly as an unknown `syscap`
  name already is.

### 7.2 The endowment-time check

`/bin/init` builds every program's authority before spawning it, and the
admission check runs there, in that order: the reservation is admitted against
the CPU it will be placed on **before** the program is started, and a program
whose reservation cannot be admitted is not started at all.

The check is arithmetic on numbers init already holds — the manifest's
reservations, the dying server's constant, and the fair floor — so a machine
whose manifest overcommits fails at the first boot of that manifest, in the same
place and with the same shape as a manifest naming a right that does not exist.

### 7.3 Refusal wording

A refusal names five things: the program, the reservation it asked for, the CPU,
what is already admitted there, and what remains. It reads as arithmetic,
because arithmetic is what the reader has to check:

> `soundd: reservation 580000/2902494 ns is 200 permille; cpu0 has 400 permille
> for real-time work and 350 are already admitted (compositor 350). Refused.`

The refusal is fatal to the program's start and is not a degraded start: a
server that runs without the reservation it was written against is a server that
will miss its deadlines quietly, which is the failure mode this whole design
exists to make impossible.

The syscall that enters the real-time band refuses with a named error when the
calling process holds no admitted reservation, and refuses a second thread of
the same process: one reservation is held by one thread at a time.

### 7.4 The build gate

The build that produces an image refuses a `system.toml` whose reservations
cannot be admitted, so overcommit is a red at build time rather than a panic at
boot. This sits beside the existing manifest gates that refuse an unknown
`syscap` right and a boot configuration missing a required program.

Because the reservation is part of the endowment rather than an argument to a
syscall, none of this is an ABI change: the syscall that enters the band keeps
its shape, and what changed is what the capability behind it means.

---

## 8. The retirer's bound, re-derived once

The retirer waits for its victim's release. Under §1.7 that wait is a function
of the dying server's reservation and nothing else that this document owns:

| term | quantity |
|---|---|
| the kick reaching the victim's CPU | one interrupt delivery |
| the preempt-off section it may land inside | the longest such section |
| the victim's own unwind | the unwind's CPU time ÷ the dying server's utilization |
| the unwinds queued ahead of it on that CPU | the same, times the queue depth |
| the phase the retire arrives in | one dying-server period |
| the pass that frees the zombie | one quantum |

With the dying server at 1 ms every 10 ms, an unwind priced at one quantum of
the victim's own CPU time costs **110 ms of wall clock**, and a victim behind
`n` others costs `(1 + n) × 110 ms` plus the fixed terms. The arithmetic is
unchanged from the number the superseded derivation reached by a different
route, which is the point: the reservation does not make the machine slower, it
makes the same number true in both directions and true for every `k`.

**The queue term stops being a constant that a queue can outgrow.** The retirer
reads the depth of its victim's CPU's unwind queue when it arms, and computes
its own deadline from that depth and the declared reservation. There is no
crossing point, because there is no constant to cross. This closes the filed
defect that the tripwire is a constant against a term the workload sets, and it
answers that filing's own objection to this remedy — that a magnitude with a
derivation attached has no kind in the kernel's duration taxonomy — because a
deadline computed by a caller from a declared rate is exactly the kind that
taxonomy already has for a caller's own arithmetic. §10 records the one thing
about that which does not yet fit.

**Two terms are not this document's, and it does not price them.**

- The pass prologue. Every scheduler pass in the chain opens by draining device
  interrupts, and that drain can reach a blocking USB path with a two-second
  bound; the number of passes in a single corpse's chain has been measured at
  twenty rather than the four the superseded derivation priced. Twenty prologues
  is forty seconds for one corpse, which no constant survives. The correct
  conclusion is not a larger constant: it is that the retirer's bound cannot be
  written honestly while a scheduler pass can block on a device, which is a
  filed defect against the pass and not a property of this wait. Until it is
  closed, the shipped constant is dominated by that defect and says so.
- The unwind's own length. One quantum of the victim's CPU time is an estimate —
  handle closes and a teardown against a pass budget — and it is declared as an
  estimate at the site. The reservation multiplies it; it does not measure it.

---

## 9. Migration

### 9.1 What survives untouched

The whole of the landed cancellation work survives. Stated explicitly, because
the value of a redesign is partly in what it does not reopen:

- **One death.** A killed thread is scheduled, not reaped: it observes the
  cancel at its next park or at its return to userland and dies by its own exit,
  with every guard on its stack given back by the thread that took them. The
  dying server is a new owner for that work, not a new way of doing it.
- **The dying list as a queue.** First-in-first-out, and never a stack. It is
  the dying server's runnable set unchanged.
- **Claim arbitration.** The kill bit set by a locked read-modify-write before
  the message is posted and before the kick; exactly one of a retire and a wake
  claiming a parked task; the loser a no-op. Untouched.
- **The completion core.** One park site, one arm, one recheck, the token, the
  borrow rule, and the uncancellable wait the retirer uses. Untouched.
- **Invariant 7's exit-boundary residual**, its one-interrupt bound and the
  ordering that residual rests on. Untouched.
- **The fair-share arithmetic** — virtual runtime, stored lag, the frontier,
  conservation of accounting — and the run queue's tie-break. It moves inside
  the fair class's reservation and is otherwise unchanged.
- **The loom models**, which model the primitives beneath all of this and none
  of the policy above them.

### 9.2 The chunks

Ordered, each landing green on its own, and none of them larger than a day of
work:

| chunk | content | size |
|---|---|---|
| **R1** | The reservation type, the per-CPU admission ledger, the three constants, and the admission arithmetic — host code and tests only, no dispatch change and no behaviour change. | small |
| **R2** | The dying server: the entity, its reservation, its queue, demotion and replenishment. Deletes §3.1's aging cluster in the same chunk, because a tree with both is a tree with two answers. Re-derives I14 and lands `old_aged_grant`. | large |
| **R3** | Real-time clients become reservation clients: earliest-deadline dispatch among entities with budget, demotion into the fair class at exhaustion, replenishment at the boundary. Re-derives I4 and lands `old_unbounded_rt_precedence`. | large |
| **R4** | Donation replaces the lent window. Re-derives the boost-window invariant and its negative gate. | medium |
| **R5** | The manifest row, the parser's four refusals, init's endowment-time check, the refusal wording, and the build gate. | medium |
| **R6** | I15 and the §6.2 scenario matrix, with the liveness fraction reported. | medium |
| **R7** | The retirer's bound re-derived at the site; the queue-shaped-tripwire defect closed; `scheduler-core-spec.md` §3, invariant 7, and the completion spec's §7.3 outline amended to their reservation forms. | small |
| **R8** | Gate A's thorough tier against the recorded sample, both audio configurations at both CPU counts, and soundd's per-period CPU time measured and written back into §4.1's budget. | measurement |

R2 and R3 are the two large chunks and they are deliberately separate: the
dying server proves the mechanism against work nobody can observe from userland,
and only then does the audio client's guarantee depend on it. R8 is what decides
whether the design ships, and it is a measurement rather than a review.

---

## 10. What resists, and is not overridden

Three things in the existing law do not fit this design cleanly. Each is
recorded rather than quietly worked around.

- **The duration taxonomy has no panicking kind with a derivation.** The
  kernel's closed set of duration kinds gives the panicking kind a constructor
  that demands why a magnitude is *absurd*, and gives the cited-magnitude kind
  an expiry that means a device broke. §8's bound is a derived magnitude whose
  expiry means the reservation was not honoured, which is a kernel bug and must
  panic. Either the panicking kind gains a second constructor taking a citation
  instead of an absurdity, or the retirer computes a deadline from a
  spec-cited bound and panics at the call site. This document requires the
  first, as three lines beside its first caller, and records that it is a change
  to a file whose whole point is that its kinds are closed.
- **Real-time wake placement moves a thread across CPUs.** The existing
  placement rule moves a woken real-time task to a sleeping peer when the waking
  CPU is itself running real-time work. Under §1.10 a reservation is admitted on
  one CPU, so that move either carries an admission check or does not happen.
  §5's placement rules therefore need amending in the same chunk as R3, and this
  document does not pretend the two are independent.
- **The spec taxonomy says a document in this directory names no file, no test
  and no chunk.** This one does all three, as its two closest siblings already
  do — the completion architecture and the log architecture specs both carry
  chunk tables, file paths and gate names in this directory. The estate has a
  de-facto second class of document living here, and this document follows the
  siblings it must be read beside rather than the rule they already broke. That
  is a discrepancy for the owner to settle, not for this document to settle by
  choosing.
