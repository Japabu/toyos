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

## The four permanent concepts

Simple means **few permanent concepts, not few lines**. Scheduling in this
estate is four of them, and nothing this design's future may add a fifth:

1. **Task and service state** — ownership, runnable/running/blocked/in-transit,
   wake, block, migrate, kill. `scheduler-core-spec.md` owns it and this
   document does not touch it.
2. **Entitlement** — how much CPU service an entity is owed, and by when:
   reservations, admission, urgency. This document.
3. **Machine capacity** — which CPUs exist, how capable each is, and what each
   costs to use. Today it is one per-CPU ledger and §1.3's statement that a
   budget is time and not work.
4. **Placement policy** — among the choices the first three leave legal, which
   one is best. That is the two seams below, and a ranker behind one of them —
   including a learned one — can cost performance and nothing else.

**Every future feature enters as data or as policy, never as a new mechanism.**
A P-core is a different capacity, a NUMA hop is a different cost, SMT is an
interference relation; EEVDF is a policy at the fair seam and a ranker is a
policy at the placement seam. A change that can be expressed as neither is a
change to this frame, and the frame changes with the owner rather than with a
chunk.

**What is implemented is the shipped machine**: four identical cores and one
real-time client — three entities, admission with refusal by name,
earliest-deadline dispatch, §1.11's timer discipline, and §1.8's mark preserving
today's pipe lend. Everything else is deferred, and each deferral is a **hook**
— one sentence here, costing nothing until somebody implements it:

- **Heterogeneous capacity** is data: a per-CPU capacity normalisation inside
  §1.3's ledger, whose open question §1.3 already states and does not answer.
- **Topology and NUMA** are a cost function at the placement seam (§2).
- **EEVDF** is a policy at the fair seam (§2), which already names it as the
  intended occupant.
- **Hierarchical shares** are reservations nesting: an entity whose budget is
  the capacity a sub-ledger is admitted against, by §1.1's arithmetic unchanged.
- **DVFS and idle states** make capacity a function of a CPU's state rather than
  a constant, which §1.1's ratio-of-wall-clock definition already tolerates.

None of the five is designed here, and a hook that grows past a sentence has
become a chunk somebody has to justify.

## The three layers, and the two seams

Scheduling in this estate is three layers, and knowing which one a question
belongs to is most of the answer:

1. **Mechanism and correctness** — `scheduler-core-spec.md`: the ownership state
   machine, who may touch a task, the park and the pass, the invariants that say
   a task is never in two places. It decides what is *representable*.
2. **Service guarantees** — this document: reservations, admission, dispatch by
   deadline, and the one invariant that says a runnable entity is never served
   below what admission priced. It decides what is *owed*.
3. **Optimization and placement** — the intra-fair policy (§2) and the placement
   policy (§2's second seam). It decides, among choices the first two layers have
   already made legal, which one is *best*.

The two seams are between 2 and 3, and they are seams rather than layers because
what is behind them is replaceable: the intra-fair policy may reorder every
fair-band thread and keep any state it likes, and a placement policy may put a
thread on any CPU whose ledger admits it. **The optimizer ranks only choices the
first two layers have already made legal, so a bad ranking costs performance and
can cost nothing else** — it cannot starve a real-time client, cannot take the
dying server's reservation, and cannot make a guarantee false, because the
guarantee was admitted before the policy ran.

## The complexity ledger

What this design costs, counted rather than asserted. Every later change to this
document updates this table, and a number that grows says what it bought.

| | before | after |
|---|---|---|
| permanent concepts | not stated anywhere | 4 |
| named constructs a reader holds to predict a dispatch | 9 | 6 |
| kernel mechanisms in this layer | 4 | 4 |
| kernel-side panic-grade assertions | 13 | 13, none of them added here |
| …of which carry a term a workload sets | 2 | 0 |
| simulator invariants | 14 | 15 |
| negative gates / must-stay-clean controls | 10 / 2 | 15 / 3 |
| sites in `toyos-sched/src` naming the deleted estate | 64 | 0 |
| production lines in `toyos-sched/src` | 6 615 | ≈ 7 200, an **estimate** |
| lines of this document | 1 899 | 2 160 |

Where each number comes from. The nine constructs before are `DYING_AGE_NS`,
`DYING_CHUNK_NS`, `Corpse::since`, `aged_grant`, `serves_rt_band`, `pick`'s
`rq.has_rt()` band test, the pipe lend's borrowed priority, the simulator's
`rt_deferral_stretch`, and `GIVE_UP`'s `1 + peers` deadline; the six after are
the reservation, the entity, the per-CPU ledger, the deadline order, the
background tier and the mark. The four mechanisms before are band precedence at
the pick, the aged grant, the pipe lend and the retirer's end-to-end deadline;
the four after are reservation-and-admission, earliest-deadline dispatch with
the background tier, the period grid (replenishment, demotion, charge-back) and
§1.8's mark. The thirteen assertions are `scheduler-core-spec.md` §2's ten
numbered invariants, invariant T (`toyos-sched/src/invariants.rs`), invariant P
(`toyos-sched/src/cpu.rs:1441`) and `GIVE_UP`
(`kernel/src/scheduler.rs:687`); the two workload-set terms are `GIVE_UP`'s
`peers = 8` and its stretch multiplier, which §8.3 replaces with fixed hops.
Gates and controls are counted from `toyos-sched/sim/tests/scenarios.rs`'s
register and from §5.5's table. The 64 sites are `grep -rn` counts for the five
deleted names in `toyos-sched/src`; 6 615 is `wc -l` of that directory, of which
the five crate gates §3.1 and §3.2 dispose of are 239 measured lines
(`cpu.rs:2263-2287` and `:2393-2606`). **The production-line figure after is an
estimate and nothing else** — no line of the reservation layer is written — and
it is the only number in this table that is not read off the tree or off this
document.

**Why each of the four concepts exists, as what breaks without it.** A concept
that cannot say what its own deletion costs is a concept somebody added, so each
witness below is a deletion argument rather than a description: it names the
property that becomes unguaranteeable, in terms that avoid the concept's own
vocabulary, and the workload that reaches it.

| concept | what becomes impossible without it, and the workload that shows it |
|---|---|
| task and service state | Two CPUs act on one thread — it is dispatched twice, or its stack is freed under it — the moment nothing decides which CPU may touch it. The workloads are in the harness: the old steal-and-scan balance path taking a task its owner was already dispatching (`old_steal_port`), and a retire and a wake both claiming one parked thread. |
| entitlement | Some legal workload starves another indefinitely, and both directions were reached in this estate: a permanently-runnable real-time thread held a CPU's unwind queue closed for ever, eleven corpses closed the rotation on the real-time band, and commit `9c2fc4d` measured 93.3 ms of audio starvation behind a fair storm. |
| machine capacity | Work is accepted against a machine that may not exist: the same numbers accepted on a core half as fast, or clocked down, buy half the work inside the same fraction of time. **This witness is weak today and the weakness is recorded rather than dressed up** — the shipped machine's four cores are alike, so no workload on it can reach the failure, and the concept earns its place from a machine the estate does not yet run (the T14's unlike cores) and from §1.3's open question about what a budget means there. |
| placement policy | CPUs that are interchangeable under everything above differ in locality, capacity and energy, and nothing above can express the difference. The tree has already paid for it: the steal probe sent a thief to the CPU holding the most *work* — one deep in two teardowns, with nothing stealable — which cost a whole idle round and broke no rule. |

**What the difference bought.** The mechanism count did not fall and the line
count rose. What fell is the number of quantities a reader has to hold to
predict a dispatch, from nine to six, and what replaced four pairwise
derivations — real-time against corpse, corpse against real-time, a stretch
factor between them, and a tripwire spending it — is one invariant that is also
one instrument (§1.9 and I15 are one predicate). Admission refuses overcommit
where the reservation is created rather than letting it appear later as a
latency. And the kill path's only assertion stops carrying a term a workload
sets. Three review rounds also *deleted* two mechanisms this document once had —
a derived bound on a blocked wait and a derived system reserve — because both
priced a quantity a workload sets, and what stands in their place is a
measurement (§1.8, §1.3). That is where a fourth round would have gone, and
review is closed here: the remaining risk is carried by the implementation's own
gates, by R8's measurements, and by the metal track.

**Why the document is longer and not shorter, since the round that ordered these
deletions expected shorter.** Two of the sections above are the additions that
round ordered — the four concepts (43 lines) and this ledger (83) — which is 126
of the 261. The rest is what deleting a derivation costs in prose: a formula is
four lines and a constant is one, while the sentences that say why there is no
formula, what is measured instead, which shape falsified each of the three
attempts, and what a reader may therefore *not* assume are ten times that. §8.1
gave back 20 lines and §3.4 and §8.2's recitals another 17, which is every line
this round was licensed to cut; what remains is arithmetic three review rounds
confirmed, and trimming that to make a line count look better is how a document
loses the derivation somebody later has to re-invent.

---

## 1. The model

### 1.1 A reservation

A **reservation** is a pair of wall-clock nanosecond quantities, `budget` and
`period`, with `0 < budget ≤ period`. Its **utilization** is `budget / period`,
carried in the kernel as **permille of one CPU** so that the admission
arithmetic is integer and exact: a reservation's utilization is
`ceil(1000 × budget / period)`, and rounding is always upward, against the
holder.

The multiply in that formula is **checked, and refuses by name rather than
wrapping**. §7.1 bounds `period_ns` from both ends so the product cannot
overflow in the first place, and the arithmetic is still written checked in both
places that run it — init's endowment check and the build gate — because an
admission test that can wrap is an admission test that can admit 1000 permille
as 78.

A reservation is held **on one CPU**, and its utilization is a fraction of *that
CPU's* capacity. It is not a share of the machine and it does not follow a
thread across a migration by itself (§1.10).

### 1.2 The entities that hold one

Exactly three kinds of entity hold a reservation, and every nanosecond a CPU
executes **on an entity's behalf** is charged to that entity — against its
budget while it holds one, and against its background service afterwards
(§1.5), but to an entity either way:

- **A real-time client.** One thread that has entered the real-time band. Its
  reservation comes from its process's endowment (§7).
- **The fair class.** One entity per CPU, holding a reservation of its own —
  §1.3's floor, 5 ms every 10 ms — rather than whatever the other two kinds
  leave. Every fair-band thread on that CPU runs inside it, and the fair class's
  internal ordering is a replaceable policy (§2).
- **The dying server.** One per CPU, kernel-owned, holding the reservation that
  serves killed threads unwinding their own stacks (§1.7).

A CPU's idle state is not an entity: an idle CPU has no runnable entity, and
unspent budget on an idle CPU is not owed to anybody. The background tier
(§1.5) is not a fourth kind either: it is these same three entities, served
after they have spent the budget of their current period.

**The nanoseconds a CPU executes on nobody's behalf have an account of their
own, and it is not an entity's budget.** An interrupt service routine and a
scheduling pass run because the machine has to, not because the dispatched
entity asked: they are charged to the **system reserve** — capacity §1.3 holds
back before admission, spent by the kernel and owed to nobody. A dispatched
entity's budget is spent by *running*, and a device that interrupts while it
runs takes from the reserve rather than from it. That is the whole of the
accounting: entity budgets, the background tier, and the reserve, with nothing
outside the three.

### 1.3 Admission

The **admission test** for one CPU is:

> the sum of the utilizations of the real-time reservations placed on that CPU,
> plus the dying server's utilization, plus the fair class's, may not exceed
> that CPU's **admissible capacity** — 1000 permille less the system reserve.

**The test is per CPU against that CPU's own ledger.** A CPU's capacity is 1000
permille *of itself*, and admission is run against what is already admitted on
the CPU a reservation is actually placed on: the ledger is the per-CPU part, and
it is the whole of it. The *arithmetic* is not per CPU. §1.1's utilization is a
ratio of two wall-clock quantities, so 580 000 ns every 2 902 494 ns is 200
permille of every core in the machine, big or little, and admission returns the
identical permille wherever it is run.

**A budget is time, and this model does not scale it by throughput.** What
differs across unlike cores is the work done inside a budget, never the fraction
of time the budget is — so a thread that needs more work per period on a little
core needs a *bigger budget* there, which is what re-running admission on a
destination is for (§2). Making a reservation mean *this much work* instead
would put a per-CPU capacity factor into §1.1's formula and change the
manifest's meaning and §7.3's refusal wording with it. This document neither
decides that nor precludes it; it refuses to claim an arithmetic it does not
have, and this sentence rather than a claim about differing fractions is what
ARM64 work implements from.

The reserve and the two kernel-owned reservations, per CPU, as fractions of that
CPU's capacity:

| quantity | reservation | utilization |
|---|---|---|
| capacity | — | 1000 permille |
| the system reserve | — | 100 permille (provisional, §9.2's R8) |
| therefore the admissible capacity | — | 900 permille |
| the fair class | 5 ms every 10 ms | 500 permille |
| the dying server | 1 ms every 10 ms | 100 permille |
| therefore the real-time ceiling | — | 300 permille |

**The reserve is held back rather than derived, and its value is provisional
until it is measured.** It pays for what §1.2 charges to it — interrupt service
routines and scheduling passes — and this document deliberately computes no
figure for that from an assumed rate. Every attempt to do so priced a pass rate
the rest of this document falsifies: §1.11's predicate forces a pass on six
kinds of event rather than on two clocks, and the dominant one — a replenishment
boundary — recurs at `1/period` of *each admitted reservation*, so the rate is a
function of the admitted set and of the machine's devices. A number derived from
one machine's clocks is a bound on nothing, and **a quantity a workload sets
does not become a bound by being written down as a constant.**

What the design states instead is the rule: **admission never fills capacity.**
The reserve is the fraction of each CPU that is never admitted to anybody; its
value here is **100 permille, provisional**; and R8 (§9.2, §9.3) measures the
reserve's actual spend on the same runs that measure soundd's budget. Until that
measurement exists, every number that rests on it — the 900-permille admissible
capacity, the 300-permille real-time ceiling, and §9.3's 870 748 ns budget
ceiling — is provisional in exactly the same way and moves with it. That is the
honest state of this number, and a reserve that was never measured is a slack
allowance with a better name.

**An overspend of the reserve is reported, never asserted, whatever produced
it.** No reserve can price an unbounded interrupt stream, and none can price a
pass rate an arbitrary admitted set forces; what the design owes is that the
overspend is *visible* — the reserve's spend per period is counted and reported
beside the demotion counts of §1.5 — and that no assertion in the kernel is
stated over it. Interrupt load past the reserve is a device defect and the
counter names the device. Passes past the reserve are this scheduler's own
overhead against a set admission accepted, there is no device to name, and the
counter names the reserve. A machine that overspends its reserve underdelivers
its entities either way, and the honest place to see that is a counter and not a
panic naming the scheduler.

**The 10 ms period is `QUANTUM_NS`** (`toyos-sched/src/fair.rs:16`), and the
choice is derived rather than inherited:

- *Bounded below by §5.3.* An entity whose period is shorter than a real-time
  client's replenishes inside that client's period and contributes a budget per
  replenishment to the client's latency term — §5.3's `ceil(P / Pⱼ)` factor,
  which is 1 exactly when the other entity's period is at least the client's.
  The fair class's period is therefore kept at or above the shortest real-time
  period the shipped machine admits, which §4 fixes at 2 902 494 ns for soundd.
  This bounds the *fair* class only: nothing here orders one real-time client's
  period against another's, which is why §5.3 carries the factor rather than an
  assumption.
- *Bounded above by what the fair band already promises.* An entity's guarantee
  window in §1.9 is its own period, so the fair period is the horizon over which
  the 500-permille floor means anything, and `scheduler-core-spec.md` §5 already
  promises the fair band one quantum as its worst accepted wake latency. A fair
  period longer than that would be this document quietly widening a promise it
  did not make.
- 10 ms satisfies both, at 3.445 soundd periods, and it is the one constant in
  the tree that already means *the horizon fair-band service is judged over*.

The direction is stated so that nobody tunes it by feel: a **longer** fair
period is a **later** fair deadline, which favours real-time latency and costs
fair-band responsiveness. Fair-internal responsiveness is §2's seam and not this
period — the seam can reorder every fair thread inside the class, and no choice
of the class's period substitutes for that.

The fair class's budget is its **floor and not a residual**: 500 permille of its
period, on every CPU, whatever else is admitted. What admission does not hand
out is **slack** — admissible capacity minus the admitted sum, per CPU — and
slack belongs to nobody. On the shipped machine (§4.1) 800 permille are admitted
of the 900 admissible, so 100 are slack and 100 are the reserve. Slack is not
lost, because §1.5's background tier is work-conserving: a CPU with only fair
work still gives the fair band the whole machine. But no guarantee rests on it,
which is what makes it the honest place to pay for everything this model cannot
price exactly — including a reserve measured to be too small, which the shipped
machine's 100 permille of slack absorbs before any admitted entity is
underdelivered. That is a statement about the shipped machine and not a
guarantee: a machine admitted to the full 900 has no slack, and its reserve has
to be right rather than generous. The floor is a guarantee and not a cap, and the same is true of
every other reservation in the table: a reservation is the least an entity gets,
never the most it may have.

**Overcommit is refused where the reservation is created, by name**, and never
observed later as a latency:

- For the two kernel-owned reservations the check is static: the fair class's
  500 permille and the dying server's 100 are constants, their sum inside the
  900 admissible leaves the 300-permille ceiling, and the constant that would
  break it does not compile.
- For a userland real-time grant the check runs at endowment, before the program
  is started (§7.2), and again at the build that produced the manifest (§7.4).

**One runtime path moves a reservation, and none creates one.** §1.10's move
releases the reservation on the source CPU and admits it on the destination at
the same period boundary, so no period is priced on two CPUs and both ledgers
are true at every instant; a move the destination cannot admit is refused in
place, in §7.3's words, and the thread stays where it is. There is no path
anywhere that creates a reservation admission did not price, which is what makes
overcommit unrepresentable rather than unlikely.

### 1.4 Dispatch

A CPU dispatches in two layers, and only the first carries a guarantee.

**The budgeted layer.** Among the runnable entities that still hold budget in
their current period, the CPU dispatches **the one with the earliest deadline**.
An entity's deadline is the end of its current period. A tie is broken by a
**total** order — first by kind (real-time client, then dying server, then fair
class), then by the entity's own key, which is the thread key for a real-time
client and the CPU id for the two kernel-owned entities, of which there is one
each per CPU. Two real-time clients admitted on one CPU can and do carry
coincident deadlines (§7.3's own example has two clients contending), so the
second half of that order is not decoration: without it "a replay of the same
choices dispatches the same entity" is not a property the rule delivers.

**The background tier.** If no runnable entity holds budget, the CPU serves the
runnable entities that hold none (§1.5), **work-conserving**, ordered by the
same virtual-runtime arithmetic the fair band already uses with each entity
weighted by its permille, and ties broken by the same total order. Background
service is slack: nothing in §1.9 rests on it and nothing in §1.9 is weakened by
it, because it is reached only when every guarantee owed at that instant has
already been met.

**A CPU with a runnable thread dispatches one.** The two layers are exhaustive
by construction — a runnable entity either holds budget or does not — so
"the pick returned nobody while something was runnable" is not a state this
model can be in, and the pick's type says so: it answers with an entity whenever
the CPU has runnable work. An entity with no runnable work is a candidate in
neither layer, and a CPU with no runnable work at all is idle (§1.2).

Which *thread* runs, once an entity is chosen, is that entity's own question:
for a real-time client the entity is the thread; for the dying server it is the
head of the CPU's unwind queue; for the fair class it is §2's policy.

§1.11 is what makes a dispatch happen at the instant one becomes owed.

### 1.5 Exhaustion, which is a degraded answer and never a silence

An entity that spends its whole budget inside one period is **demoted to the
background tier for the remainder of that period** (§1.4). It does not stop, it
is not requeued at the back of anything, and it is not preempted merely for
having been demoted: it keeps the CPU until an entity that still holds budget
becomes runnable, and it is served again whenever none is.

**Demotion is one rule for all three kinds.** A real-time client that overran, a
dying server on a CPU with nothing else to do, and the fair class that has spent
its own 5 ms are all in the same tier, ordered by permille. The previous form of
this rule — "demoted into the fair class" — was circular for the fair class
itself, and it charged one entity's overrun to another entity's budget. The
background tier is neither: it is outside every budget, so nothing an exhausted
entity does can be taken out of an entity that still has one.

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
- **A waking entity may spend at most what its reservation prices before its
  deadline** — `utilization × (deadline − now)`, and never more, which for a
  wake at a boundary is the whole budget and for a wake late in a period is a
  fraction of it. Without this an entity that woke 100 µs before its boundary
  would hold a whole period's budget against a 100 µs deadline, win every EDF
  comparison on the CPU, and then refill: the stale-deadline overcommit the
  bullet above forbids, wearing its other face. Both rules exist so that the
  demand inside any window is the demand admission priced for that window, which
  is what §1.9's derivation rests on.
- **An overrun is charged to the next period.** A CPU cannot take itself away
  from a running entity faster than it can take an interrupt, so an entity can
  run past its budget by at most one delivery granularity (§1.11). The refill at
  the next boundary is `budget` less that overrun, and the deficit cannot
  compound because it is paid in full at the first boundary after it happens. A
  granularity that were forgiven instead of charged would be a standing gift:
  200 000 ns of `MAX_PASS_NS` in each 2 902 494 ns period is 68.9 permille of a
  CPU that admission never priced. **The subtraction is total because `budget`
  is floored at `G`**: §7.1 refuses `budget_ns < G` by name, for the same reason
  it refuses a period below it, so `budget − overrun` is never negative and
  "paid in full at the first boundary" is arithmetic rather than an intention.

**The boundary grid's phase origin is admission.** An entity's first boundary is
the instant its reservation was admitted on its CPU — endowment time for a
manifest reservation (§7.2), the CPU's own start for the fair class and the
dying server — and every later boundary is that instant plus a whole number of
periods. Nothing else moves the grid: not a wake, not a demotion, not a kill,
and not a move (§1.10 re-admits at a boundary precisely so that the destination
grid begins where the source grid ended). Entities admitted at different
instants therefore carry different phases, which is one reason §1.4's tie-break
needs a total order, and it is what lets an instrument reconstruct any entity's
whole grid from an admission instant and a period.

**"Exactly one full budget per wake" is an idealisation, and this is its
condition.** A client whose reservation period matches the rate it is woken at
gets exactly one full budget per wake *when the two clocks agree*, and they do
not: the wakes ride the device's own oscillator and the boundaries ride the
kernel's wall clock on a grid anchored at admission. The condition that makes
the idealisation harmless is not zero drift — it is **headroom at least the
per-period relative drift, priced at the cap**. If a wake lands `s` before a
boundary, the bullet above lets the entity spend at most `u·s` of that period,
so at most `u·s` of the mix happens before the boundary and the rest — `C − u·s`
— happens on the next period's full budget. A period therefore has to deliver
one mix's worth of service plus whatever a straddling predecessor left it, and
the condition is `B ≥ C + u·d` for a per-period relative drift `d`. With
`B = 580 000` and the mix cost `C ≈ 203 175` (§4.1, and historical), that is
`203 192` ns against a budget of 580 000: the headroom is 376 825 ns against
17.4 ns of capped drift charge at 30 ppm — 21 638× — so the idealisation holds
on the shipped configuration and fails only for a budget sized to the mix cost,
which §4.1 forbids and §9.3's write-back transform goes on forbidding: a factor
of two leaves a headroom equal to the measured cost itself.

Both beats are computable and neither is hidden. From the manifest's truncation
alone the grid period 2 902 494 ns is *shorter* than the device's
2 902 494.331 ns, so wakes walk later against the grid and a grid period
occasionally receives no wake and never two: one full period of slip every
8 767 122 periods, 7.07 h. In the other direction a codec crystal fast by 50 ppm
puts two wakes inside one period every 20 047 periods, 58.2 s; the second of the
pair lands `d` = 145 ns before the boundary, may spend at most `u·d` = 29 ns
there, and does its whole mix on the budget the boundary refills. It never runs
on `B − C`: that would be the uncapped machine the bullet above abolishes, where
a wake arriving 145 ns before its deadline could hold a whole period's budget
against it.

### 1.7 The dying server

A killed thread unwinding its own stack is served by the **dying server**, an
ordinary reservation client that happens to be owned by the kernel rather than
by a process.

- Its runnable set is the CPU's queue of killed threads, served
  first-in-first-out. A killed thread is never migrated, so the queue it stands
  in is the queue of the CPU that owned it.
- **The queue is the runnable set, and a wait is not in it.** A corpse that
  parks — on a sleep lock, or inside a device wait — leaves the queue for the
  duration and rejoins at its *tail* when it wakes. That is the tree's own
  arrangement and this document changes none of it: the dying list holds ready
  tasks and is pushed at the back (`toyos-sched/src/cpu.rs:672`), the pick takes
  the front (`:1256`), and a parked task is in `parked` (`:191`) until a wake or
  a retire returns it. Three things follow and each is used later: "the dying
  server's queue is non-empty" and "the dying server has runnable work" are one
  statement, so §1.9 needs no special case for a corpse in a wait; a period in
  which the only corpse is parked is a period the server was not runnable in and
  is owed nothing for; and §5.4's head-relative interval is an interval of
  *runnable* time, which a wait ends rather than inflates.
- **Whether a corpse can park at all is R2's question, and this document does
  not answer it.** `WaitTicket::commit` refuses to park a killed task on any
  wait that answers cancellation (`toyos-sched/src/waitq.rs:463-467`), which is
  what keeps it running on its own stack, so the parked-corpse state is reachable
  today only through an uncancellable teardown wait and an in-unwind device wait
  is still a spin. The rule above is stated for the arrangement the tree will
  have once `completion-architecture-spec.md`'s C7 lands; where the state is not
  reachable, nothing in this document is weaker for it.
- It is dispatched, preempted, demoted and replenished by §§1.4–1.6 and §1.11,
  with no rule of its own. It has no age, no stamp, no grant and no chunk.
- Its reservation is a **floor, not a cap**: on a CPU with nothing else runnable
  the dying server exhausts its budget, is demoted to the background tier, and
  keeps running there — so an unwind on an idle CPU is not slowed down by the
  existence of a reservation for it. On a CPU whose real-time clients are all
  spending their full budgets, it still receives 1 ms of every 10 ms, because
  admission guaranteed there was room for it.

The two failure shapes that the previous designs alternated between are both
unrepresentable: real-time work cannot hold the dying server below its
reservation, because the reservation was admitted against real-time's own; and
the dying server cannot hold real-time work below *its* reservation, because
budget is the only thing it can spend and it has 100 permille of it.

### 1.8 One mechanism: the urgency mark

The lent band is replaced by **one** mechanism, and it moves no budget at all.
Two shapes of urgency exist — a real-time waker that needs its wakee to run, and
a waiter that needs the holder of a resource to finish — and the previous form
of this section answered them with two accounting operations: a grant that
charged the wakee's time to the waker's budget, and a donation that lent the
waiter's budget to the holder. Both were drainable, because both let one party's
service depend on another party's behaviour. The mark answers both shapes
without moving a nanosecond of anybody's budget.

**What sets a mark.** Exactly two events, and nothing else:

- a real-time client's wake targets a thread that holds no reservation of its
  own — `soundd` signalling a client (`audio-subsystem-spec.md` §4);
- an entity blocks on a resource a thread holds — priority inheritance.

**What a mark does.** A marked thread is dispatched **ahead of unmarked threads
inside its own service class**. That is the whole of the effect. The mark is
invisible to §1.4: it never changes which *entity* the CPU dispatches, never
moves a deadline, and never creates or resizes a reservation, so §1.3's
admission sum is untouched by it.

**What a mark costs, and who pays.** The marked thread spends **its own class's
budget, never the marker's**. There is no pot, no transfer, no double spend and
no cross-CPU accounting, so the question the previous two mechanisms both had to
answer — whose account is charged — is not asked anywhere. A hostile or spinning
wakee can consume only its own class's share, which is what lets §4.1's audio
bound stop depending on client behaviour.

**Marks are totally ordered.** By the instant the mark was set, first set first
served, ties broken by thread key. A class dispatches the head of its mark queue
before any unmarked thread. Without a stated order among marked threads, `k`
marks in one class divide its service by a quantity the workload sets, and the
head of the queue is decided by a race rather than by a rule.

**The marked half.** In each of its own periods a class may deliver at most
**half its budget** under marks; past that the marks confer no precedence for
the remainder of that period and the class's ordinary policy decides, which is a
cap on precedence and never an idle CPU. Half is the largest cap at which the
service a class delivers *under marks* never exceeds the service it delivers
outside them — a statement about service and not about threads, because a marked
thread also draws its ordinary share of the unmarked half and therefore does
take more than an unmarked peer. What the cap bounds is how much of a class
marks can move, which is why §2 measures per-process fairness over the unmarked
half and reports the marked service per process beside it rather than treating
the cap as making marks invisible to fairness. For the fair class the marked
half is **250 permille** — 2.5 ms of every 10 ms — and that is the rate a
cross-process mark buys.

**A mark ends** at the first of: the wait that set it ending, the marked thread
blocking, or the mark's window — `RUN_CHUNK_NS` = 1 ms **of running time**, for
both shapes of mark. One window in one clock is deliberate: the previous form
gave a hold mark the lock's declared hold bound as its window, which required
that bound to be CPU time while the only bound a lock can honestly declare is a
wall-clock device timeout, so the two readings differed by the class's own rate
and each mark cost the class a different amount. `RUN_CHUNK_NS` is the
*simulator's* step granularity (`toyos-sched/sim/src/vm.rs:46`) and not a kernel
constant; R4 introduces the kernel's own and this document is where its value is
stated, the same way §1.9 names the simulator's `IPI_LATENCY_NS` for the hop it
prices. A thread cannot re-mark itself: a fresh mark takes a fresh wake or a
fresh waiter, and the party that raises one is the party that loses by waiting.

**A mark inside one process buys that process nothing.** A mark whose waiter is
a thread of the marked thread's own process orders the holder ahead of that
process's *own* threads and no further. Per-process fairness makes the process
the unit of share, so a wait internal to one unit cannot move that unit's share
— and this is the shape the code makes cheapest, since `ProcessData` is per
process with 68 `with_fd_owner_data` sites (`completion-architecture-spec.md`
§9): a sibling blocking on a sibling is the easy farm, and it is exactly the
farm that pays nothing. Two *different* processes passing a lock between them
can keep one mark alive between them; they take at most the marked half, they
accrue virtual runtime for every nanosecond of it, and the marked service a
class delivered is counted per process and reported. That is priority
inheritance's standing price — bounded, attributed and visible — and it is the
residual this design accepts rather than hides.

**Nobody moves.** A mark never migrates a thread, so §1.10's pinning and
invariant 7's never-migrate rule are untouched, and it carries no bandwidth
across a CPU: a marked thread is served by its own CPU's class at that class's
admitted rate, wherever the waiter or waker is. The same-CPU and cross-CPU cases
therefore give the *same* answer, which the two-mechanism form could not say.

**It is transitive.** If the holder is itself blocked on a further holder the
mark follows the chain; the kernel's sleep locks are a fixed, ordered set —
`{ProcessData, VFS, VOLUMES, XHCI}`, `completion-architecture-spec.md` §9 — so
no cycle exists and no chain is longer than four links.

**The wait a blocked real-time client suffers is measured and reported, and this
document states no bound for it.** Three forms of this design derived one and
each was falsified by an ordinary workload: the first divided a hold bound by
the class's whole utilization, and one concurrent holder broke it; the second
divided by the marked rate, and `k` concurrent marks broke it; the third carried
`k` explicitly and was broken by the shape the tree actually produces. That
shape is worth naming, because it is the only one the shipped machine has:
`ProcessData` is per process (`completion-architecture-spec.md` §9), every
`with_fd_owner_data` site locks the *calling* process's copy
(`kernel/src/process.rs:960-968`), and §1.8's own same-process rule pays such a
mark nothing — so a real-time thread blocked on its own process's lock waits for
a sibling served at that process's share of the fair class, which the number of
runnable fair processes sets. **A quantity a workload sets is not made into a
bound by being moved one level down** — out of a kernel assertion and into a
formula, a harness gate or an admission constant — and the honest half is what
stays here.

- **The kernel counts the interval an entity spends blocked on a sleep lock**
  and reports past an expectation it computes from what it holds at the block:
  the lock's declared hold bound at the holder class's marked rate, plus one
  period of that class, plus the marks already queued ahead. For a cross-process
  fair-band holder of a lock declaring a 2 ms bound that is `2 ÷ 0.25 + 10` = 18
  ms, with 4 ms per queued mark. It is an expectation about a composed quantity,
  so it names the lock, the holder's class and the mark's fate, and **nothing is
  asserted over it**.
- **The harness measures the same interval** in §6.2's lock cells and reports
  its distribution beside I4 rather than gating on it (§5.3).
- **The per-lock obligation stays**, below, because it is the half that is cheap,
  static and checkable.

**Every sleep lock in the fixed set declares a bound on how long it may be
held**, in its own contract, and **a lock that declares none may not be taken on
a path where a real-time client can block on it.** That is the obligation this
mechanism puts on the sleep-lock set, and it is checkable rather than hoped for:
the locks are four, the real-time paths are few, and the question "can an
admitted real-time client block on this lock" is a static one. The rule bites
`VFS` first, and it should: a `VFS` holder may be inside device I/O, so any
bound it can honestly declare is a device timeout — `USB_TIMEOUT_NS` = 2 s
(`kernel/src/drivers/xhci/mod.rs:319`) — which no real-time path can accept.
**The design constraint is therefore stated rather than a bound invented: no
real-time client's path may take `VFS`**, which is true of `soundd` today (its
cycle opens no file and reaches no filesystem call — `userland/soundd/src`
contains no `Vfs`, `fsync` or file-open site). `ProcessData` is the second row
the rule reaches and this document does not settle it: soundd's real-time thread
takes that lock on every period through its poll and read calls, while a holder
inside an `fsync` parks under `{VFS, VOLUMES, XHCI}` — so either the lock
declares a bound its own paths meet or the rule bites the shipped audio client.
That is recorded in §10 as the owner's question, and R4 is where it is asked
statically rather than assumed. Where the per-lock bounds are declared is
`completion-architecture-spec.md` §9's own lock table, not here (§10).

The expectation above is 6.2 device periods, so a real-time client that takes a
sleep lock inside its own period is a client whose design is wrong, and the
report exists to make that a number rather than a dropout. What this document no
longer claims is that the wait has a bound the scheduler delivers: it has a
declared hold bound at one end, a measurement at the other, and a design rule in
between.

**The mark is an input to §2's seam, and the seam is amended to take it.** The
reservation layer hands the intra-fair policy an ordered mark queue and the
marked half; the policy dispatches the queue's head before any unmarked thread
until the half is spent. That is an ordering input and not a reservation: the
policy still cannot read a budget, a deadline or a permille, and a marked thread
accrues virtual runtime for every nanosecond it runs like any other.

### 1.9 The invariant

> **No runnable entity is served below its reservation.** For every entity, over
> every one of *its own periods* in which it was continuously runnable, the CPU
> time it received is at least its budget, measured on the wall clock.

This is the whole of the liveness claim, and it is stated in the same words
§6.1's I15 tests, deliberately: the law and the instrument are one predicate, so
there is no reading left for a harness to choose. The window is the entity's own
period grid (§1.6's phase origin), not a window that slides.

**It is delivered because admission and dispatch are the two halves of one
result.** §1.3 keeps the sum of the utilizations on a CPU at or below its
admissible capacity, §1.4's budgeted layer is earliest-deadline-first over
reservations whose deadlines are their periods, and §1.6's rejoin rule stops an
idle entity from carrying a claim into a later period. That is the classical
implicit-deadline EDF result: at a utilization sum of at most one, every entity
meets every deadline, which for a server means it receives its whole budget
inside every period it can use it in — and the sum admission enforces is at most
900 permille rather than 1000 precisely so that the machine's own overhead is
not spent out of it (§1.2). Whether the 100 permille held back is *enough* is a
measurement and not a derivation (§1.3): §1.9 guarantees delivery against the
admitted sum, and it is never a claim that the reserve was priced right. The
background tier cannot weaken it because it runs only when no entity holds
budget and is runnable.

**The sliding window gets the weaker guarantee, and it is stated here so that no
later reader upgrades it from memory.** Over an arbitrary window of length
`period`, an entity continuously runnable throughout is guaranteed
`max(0, 2·budget − period)` and no more. The derivation: a window of length `P`
starting `x` into an aligned period covers `P − x` of that period and `x` of the
next; of the first period's `B`, at most `min(x, B)` can have been delivered
before the window, leaving at least `B − x` inside it; of the second period's
`B`, at most `min(P − x, B)` can be delivered after the window, leaving at least
`B − (P − x)`; the sum is `2B − P`, independent of `x`. For soundd's
`580 000 / 2 902 494` that floor is zero, and a legal EDF schedule reaches it —
service at the head of one period and the tail of the next leaves a full period
with none. **Nothing in this document rests on the sliding form**: §4.1's
wake-to-run bound is derived from the aligned guarantee and §1.4's deadline
order (§5.3), which is why the correction costs an argument rather than a
promise.

**One delivery granularity is carried, not pretended away.** A CPU acts on an
exhaustion or a boundary at the next pass it can take (§1.11), so service can be
late by at most `G`, and an entity can overrun by at most `G`, which
§1.6 charges back at its next boundary. The guarantee above therefore carries
one `G` of tolerance, once, and never once per period: §6.1's I15 compares
*cumulative* service against *cumulative* owed budget for exactly that reason,
so a scheduler that spends `G` every period accumulates a deficit and reds
instead of hiding inside a per-period allowance. `G` is 34.5 % of soundd's
budget and 20 % of the dying server's, which is why it is a term this document
states rather than a rounding error it ignores.

**`G` is `MAX_PASS_NS` = 200 000 ns (`toyos-sched/src/cpu.rs:893`), and it is
one quantity rather than a sum.** It is the interval from the instant an event
becomes true to the end of the pass that acts on it, and the pass is what the
kernel already bounds: invariant P asserts the elapsed time of every pass
against `MAX_PASS_NS` (`toyos-sched/src/cpu.rs:1441`) and the negative gate
`overlong_pass` holds that bound. What stands in front of the pass — the local
interrupt's own delivery — is hardware latency in the microseconds, an estimate
and the only term in this document not read off the tree, and it is an order
below the bound that follows it; the document names it, declares it dominated,
and spends `MAX_PASS_NS` wherever `G` appears rather than carrying a sum with an
unmeasured addend. The **cross-CPU** hop is not inside `G`: §1.11 puts the
message and the kick in front of it, and I4 prices that separately at the
simulator's own `IPI_LATENCY_NS` = 200 000 ns
(`toyos-sched/sim/src/vm.rs:52`).

Starvation is not bounded by any of this — starvation is unrepresentable under
it, because an entity that could be starved is an entity whose reservation was
admitted and therefore is not.

The wall clock is the measurement, deliberately: it is the clock I15 and I14 are
evaluated on and the clock §8's reports are stamped with, and a model that
measures liveness on a clock the kernel cannot read is a model that cannot see
the failure the kernel dies of.

### 1.10 Reservations and placement

A thread holding a real-time reservation is **pinned to the CPU its reservation
was admitted on**. A move requires releasing the reservation on the source CPU
and admitting it on the destination, and a move whose admission fails does not
happen — the thread stays where it is rather than moving and losing its
guarantee. A refused move is reported in §7.3's words with the destination's
ledger in it, and it is not fatal: nothing was lost, because nothing moved.

**A move happens at a period boundary, and exactly one period's budget exists
across it.** The release on the source and the admission on the destination are
the same instant, chosen as the reservation's next boundary: the source's ledger
loses the utilization from that boundary, the destination's gains it there, and
the moved entity's first period on the destination begins with a full budget
because it is a first period and not a continuation. A move that admitted fresh
budget mid-period would mint time neither CPU's admission priced, and one that
carried a partial budget across would need entity state to cross a CPU, which
§1.6's boundary rules do not describe and this model does not need.

**The arrival is a message and a kick, like a wake.** The destination CPU could
not have armed for the boundary of an entity it did not yet own, so the move
tells it the way every other cross-CPU event does, and §1.11's preemption
predicate lists the arrival among the events that force a pass. Without that the
moved entity would sit with a full budget and, typically, the earliest deadline
on its new CPU until something unrelated happened to run a pass.

The fair class and the dying server are per-CPU by construction and do not move.
Fair-band threads move freely: they are inside the destination CPU's fair class
when they arrive, and the fair class's reservation is not a function of how many
threads are in it.

### 1.11 The clock: what arms the timer, and what forces a pass

§§1.4–1.6 say what dispatch, exhaustion and replenishment *mean*. This says what
notices them, because a rule nothing observes is not a rule: an exhaustion seen
one quantum late is an entity that spent 10 ms of a 580 µs budget, and a
replenishment seen one quantum late is an audio client three device periods
behind its own wake.

**The one-shot timer arms at the earliest of four instants**, and this list
amends the timer discipline `scheduler-core-spec.md` invariant 6 states rather
than leaving it alone:

1. **The running entity's exhaustion instant** — now plus its remaining budget.
2. **The earliest boundary that can change the winner** — the running entity's
   own deadline, and the boundary of any runnable entity that is out of budget,
   because replenishment is what makes it a candidate again. A boundary of a
   runnable entity that still holds budget cannot change the winner: its
   deadline only moves later, and it already lost.
3. **`quantum_end`**, which is the fair class's internal slice and belongs to
   §2's policy, not to this layer.
4. **The earliest parked-task deadline**, which is invariant 6 unchanged.

**The preemption predicate.** An event that makes a different entity the winner
of §1.4's budgeted layer forces a scheduling pass on that CPU: a wake, a
replenishment boundary, an exhaustion, a kill, the arrival of a corpse, and the
**arrival of a moved reservation** (§1.10). The pass lands within `G` of the
event — `MAX_PASS_NS` = 200 000 ns (`toyos-sched/src/cpu.rs:893`, and §1.9 for
why `G` is that one quantity) — and for an event raised on another CPU the
message-and-kick hop is in front of it, which is the term invariant I4 already
prices. This is the rule §1.7 cites when it says the dying server is preempted
by §§1.4–1.6 with no rule of its own; before it was written, that citation
pointed at three sections that contained no preemption rule at all.

**What this costs and where it is paid.** `G` is the whole of the difference
between the model and the machine: it is why §1.6 charges an overrun back at the
next boundary, why §1.9 carries one tolerance rather than one per period, and
why §7.1 floors both coordinates of a reservation at it. The *pass itself* is
paid for out of §1.3's system reserve rather than out of the dispatched entity's
budget, and how much of the reserve the passes take is measured rather than
derived: the list above makes the pass rate a function of the admitted set, its
periods and the machine's devices, which is exactly why §1.3 holds a fraction
back instead of pricing one. `G` is not a term any workload scales — one pass,
once per event, whatever `k` is — and that is a statement about a pass's
*length*, never about how many of them a second contains.

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
  as a whole receives, and may not create an entity. **It takes exactly one
  ordering input from the reservation layer**: §1.8's mark queue — a totally
  ordered list of marked threads, and the marked half of the class's budget
  within which the policy must dispatch that list's head before any unmarked
  thread. That is an order and a cap, not a reservation: the policy still cannot
  read a budget, a deadline or a permille, and a marked thread is charged for
  what it runs like any other. Anything else it does is therefore invisible to
  §1.9: no intra-fair policy can starve a real-time client or the dying server,
  and none can be starved by them, because the fair class's floor was admitted
  before the policy ran.
- **Per-process fairness is measured outside the marked half, and the marked
  half is measured too.** I5 compares processes over the service the class
  delivered while no mark was being honoured; the marked service is counted per
  process and reported beside it. A class that delivered more than half its
  budget under marks reds I5, which is what gives §1.8's cap teeth, and a
  process whose *unmarked* share is below its entitlement reds it for the
  ordinary reason. Without that split a mark would be invisible to the one
  invariant that can see it being farmed.
- **The background tier is on this side of the seam and its ordering is not.**
  §1.4's background order across *entities* is the reservation layer's own — the
  weights are permille, which the policy may not read — and the policy is asked
  only which fair-band thread runs when the entity chosen there is the fair
  class. The same split holds in the budgeted layer, so the seam has one shape
  in both.
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
the ledger is per CPU is what keeps that true: a placement policy that moves a
thread between unlike cores re-runs admission against the destination's ledger,
and what it gets back is that CPU's own arithmetic rather than an assumption
about the machine. What such a policy would additionally need — a budget that
means the same *work* on unlike cores — is §1.3's named open question and not a
thing this model quietly supplies. Nothing about that policy is designed here.

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
- `preempt_if_due`'s `!aged_grant` term in the real-time preemption test, and
  the band test around it. §1.11 states the preemption predicate that replaces
  both: an event that changes the winner of §1.4's budgeted layer.
- The dying list's field doc paragraph as the tree now holds it (`cpu.rs:216`):
  the aged-rule statement ("serves `rq` first *unless the head of this list has
  aged*", with `preempt_if_due`'s grant beside it), the struck-history quote it
  carries, and the "**Superseded in design by**" paragraph that names this
  document (`cpu.rs:228`). All three go together, replaced by a field doc that
  says what the list is under §1: the dying server's runnable set, FIFO, served
  at a declared reservation. The paragraph the previous form of this bullet
  described — the one asserting the pick serves `rq` first whenever the
  real-time band is occupied — is already struck history in the tree and is not
  a deletion target anybody can find.
- The `rt_outranks_every_corpse` escape hatch, replaced by §5.5's stronger pair.

**The four crate-level gates in `cpu.rs`'s test module, each dispositioned by
name.** They compile against `DYING_AGE_NS` and `DYING_CHUNK_NS`, so they go
with the constants, and each is replaced in the direction it guarded:

| gate | disposition |
|---|---|
| `a_killed_task_does_not_starve_a_ready_rt_task` (`cpu.rs:2263`) | **re-derived.** The property survives whole; what changes is the number. A ready real-time client waits at most the dying server's budget for a corpse in front of it, and not one `DYING_CHUNK_NS` grant. `scheduler-core-spec.md`:177 names this gate as normative law and is amended with it (§3.3). |
| `a_corpse_is_not_starved_for_ever_by_a_spinning_rt_task` (`cpu.rs:2393`) | **replaced by `a_corpse_gets_the_dying_server_s_reservation_under_a_spinning_rt_task`**, asserting the same direction against §1.9: over ten dying-server periods with a permanently-runnable real-time client on the CPU, the corpse receives at least 1 ms in each. Its old assertions — one chunk per age window, and every aged dispatch exactly one chunk — have no referent under §1. |
| `an_unwind_under_saturated_rt_is_stretched_by_the_age_ratio` (`cpu.rs:2494`) | **replaced by `an_unwind_under_saturated_rt_is_stretched_by_the_reservation`**, asserting §5.4's arithmetic instead of the 11× ratio: the wall-clock stretch is `period ÷ budget` = 10 exactly, and a change to either dying-server constant that widened it reds here. This is the anti-drift gate `invariants.rs:134` describes, kept pointed at the new rate. |
| `an_aged_grant_is_not_undone_by_a_pass_inside_its_chunk` (`cpu.rs:2521`) | **replaced by `an_interrupt_storm_does_not_take_the_dying_server_s_budget`.** The grant it protects does not exist, but its subject does: passes are handed out by every interrupt the machine takes, and the replacement asserts that a stream of them inside a dispatched entity's budget does not cost that entity its *budget*. The name is what it asserts, because §1.2 gives that time an account of its own: budget is spent by running, and an ISR and its pass are charged to §1.3's system reserve. What the gate cannot assert, and does not, is that a storm costs the entity no *service* — a machine that interrupts past its reserve delivers less to everybody, which §1.3 reports and nothing panics on. |

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
- `Op`'s vocabulary gains `Acquire` and `Release` rather than losing anything:
  §1.8's waiter and holder are unrepresentable in a script that has no
  resource-holding op, so `old_park_kept_the_lend`'s re-derivation,
  `old_uncapped_mark` and the blocked-wait measurement of §6.2's lock cells
  cannot be staged until they exist. R4 owns that (§9.2).

`toyos-sched/src/task.rs`

- `RunningTask::serves_rt_band` (`:717`) and the doc paragraph above it
  (`:700-716`), which cites `DYING_AGE_NS` as "what keeps that from starving"
  and names `a_killed_rt_thread_unwinds_in_the_normal_band` as its gate. §1.4's
  pick asks which entity has the earliest deadline with budget, so the question
  the method answers is not asked anywhere; the method, its doc and its gate go
  together, and the property the gate held — a dying thread is not real-time
  work by virtue of a right it still holds — is carried by membership in the
  dying server instead (§5.1). The intra-doc link to the deleted constant
  survives compilation and only warns under `cargo doc`, which is why this site
  is named here rather than left to the toolchain.
- **The method has three callers and the deletion lands with the last of them,
  at R3.** `preempt_if_due`'s band test (`toyos-sched/src/cpu.rs:1166`) goes at
  R2 with §3.1's cluster; the crate gate (`cpu.rs:2564`) goes with the method;
  and the third is the *simulator's* current I4 predicate
  (`toyos-sched/sim/src/invariants.rs:463`, `rq().has_rt() && …
  !task.serves_rt_band()`), which §5.3 re-derives. R2 therefore leaves the
  method standing with its doc struck rather than breaking the sim build, and
  R3 deletes method, doc, gate and predicate together — a tree with the method
  and no caller for one chunk is not a tree with two answers, and a tree with
  a sim that does not compile is not a chunk that lands green.

### 3.3 The pairwise liveness derivations

`specs/scheduler-core-spec.md`

- §3's qualification paragraph — the age window, the one-chunk grant, the
  restarting stamp, the "at most 1 ms per 10 ms", and the "one chunk per 11 ms"
  delivery rate — and the two "both absolutes were tried" bullets under it.
  §5.1 states what replaces them.
- The paragraph naming `a_killed_task_does_not_starve_a_ready_rt_task` and
  `a_corpse_is_not_starved_for_ever_by_a_spinning_rt_task` as the normative gate
  pair (`:177-180`). It is amended rather than struck: the pair becomes the pair
  §3.1 dispositions, and the sentence names them.
- The `RunningTask::serves_rt_band` paragraph (`:143-151`) — "the one question
  both halves ask". Under §1.4 neither half asks it: the pick asks which entity
  has the earliest deadline with budget. §5.1's replacement text is what the
  paragraph becomes.
- Invariant 7's release clause insofar as it prices the unwind through the
  stretch factor. §5.2 states what replaces it.
- The pipe-lend paragraph (`:182-185`), which promises a lend of a *band* for
  one quantum. It is amended to §1.8's urgency mark — precedence inside the
  reader's own class, bounded and unpriced in budget — and the amendment is
  already recorded at the site so that the surviving law and this document do
  not disagree while R4 is unlanded.

`kernel/src/scheduler.rs`

- `GIVE_UP`'s elevenfold-stretch term, its `peers = 8` pricing, and its
  end-to-end shape. §8 states what replaces them: the terms that survive are the
  fixed hops, and the end of the wait is supervised on the victim's CPU rather
  than deadlined from the retirer's.

`specs/completion-architecture-spec.md`

- §7.3's outline of the derivation, insofar as it restates the four pass
  prologues, the 11× factor, and `peers = 8`. §5.6 records the reconciliation.
- §7.2a's citation of `rt_deferral_stretch()` and of `retire_latency_bound`'s
  stretch term (`:1347`). The amendment that section records is not reopened;
  the two names inside it are deleted by §3.2 and the sentence carrying them is
  re-stated against §5.4's rate.
- **§22's two surviving rows** (`:3192`, `:3194`). The first states aged-grant
  law as current — `SchedPass::pick` dispatching a corpse "ahead of the
  real-time band once it has aged" — and becomes §5.1's sentence: the pick asks
  which entity has the earliest deadline with budget. The second records
  `GIVE_UP`'s 10 s end-to-end deadline as discharged with "the residual `peers`
  term … filed": the deadline is deleted by §8.3 and the term by §5.4, so the
  row becomes the fixed-hop tripwire and the filed defect closes. Both land at
  R7 with the rest of §8's site work.

`specs/audio-subsystem-spec.md`

- §4's lend — the "under the lent band" phrase and the sentence that replaced
  it, which promised soundd's precedence "for the duration of its fill", a
  whole-fill guarantee no mechanism in this document delivers. **Already deleted
  at the site**, in the same pass as the scheduler-core amendment above: §4 now
  says the wake marks a signalled client urgent, names §1.8, charges the fill to
  the client's own class, and states what the mark does *not* promise. The
  mechanism abolished there is the lend and §1.8 is what abolishes it; this row
  records a deletion that has happened rather than one that is owed.

### 3.4 The three findings this design answers, at the statements that were false

The adversarial pass that preceded this document confirmed three unsound
statements about bounded deferral. Each is answered by a rule above rather than
patched, and the story of how each was found is in that pass's own record:

- **Bounded deferral was per corpse, not per CPU** — `k` corpses took `k`
  grants, and at `k ≥ 11` the rotation closed on itself. §1.7's dying server is
  one entity with one reservation however many corpses it holds.
- **The stretch factor bounded nothing in the other direction** — a briefly
  empty real-time band restamped the corpse's age and threw its accumulated
  claim away. §1.6 replenishes on a boundary the workload cannot move, and there
  is no stamp to restart.
- **The real-time latency term was a `k = 1` term** — one `DYING_CHUNK_NS`.
  §5.3 re-derives it against the reservations whose deadlines are earlier, and
  the re-derived term is **larger** — 2 322 494 ns against 1 ms on soundd's CPU.
  The old number was smaller because it priced one corpse and was false for
  every `k` above one; calling the new one tighter would be this document
  flattering itself.

---

## 4. The audio contract

### 4.1 soundd's reservation

`soundd` holds a real-time reservation of **580 µs every 2.902494 ms** —
19.98 % of one CPU, admitted as 200 permille.

**The period is the device period to the nanosecond the manifest can hold.** 128
frames of stereo 16-bit audio at 44 100 Hz is 128 ÷ 44 100 s =
2 902 494.331 ns, and `period_ns` is an integer, so the declared 2 902 494
truncates 0.331 ns of it — a grid 0.11 ppm fast against an ideal device clock,
which §1.6 prices: the wake walks later, one full period of slip per 8 767 122
periods (7.07 h), and the mix that straddles a boundary is covered by the
headroom that section derives. "Exactly" was the wrong word for a quantity
integer nanoseconds cannot represent.

**What the matching period buys is stated against §1.9's aligned form**, because
that is the only form the design delivers, **and the derivation's premise is
that the wake lands at a boundary.** For a wake at a boundary soundd is
continuously runnable from there until its mix is done, so §1.9 gives it its
whole 580 µs inside that period; therefore its first dispatch is no later than
`period − budget` = 2 322 494 ns after the wake, and its mix is complete by the
next boundary, whatever else is admitted on the CPU — those numbers come from
the guarantee rather than from the interference sum, and §5.3's sharper term is
the same bound arrived at from the other side. That is what "bounded by one
device period rather than by a quantum" means: 2 902 494 ns against
`QUANTUM_NS` = 10 ms, which is 3.445 device periods.

**The premise is not delivered by any rule in this document, and the shortfall
is stated rather than assumed away.** §1.6 anchors the boundary grid at
admission and nothing moves it, while the wake stream rides the device and is
re-anchored whenever the pipeline drains — the device "restarts its period grid
from whatever is submitted next" (`audio-subsystem-spec.md` §2). The relative
phase `φ` of wake to boundary is therefore an arbitrary value in `[0, P)` fixed
by when the stream started. What the design still delivers at `φ > 0`, derived
the same way: the period a mid-period wake lands in owes soundd nothing, because
it was not runnable throughout it, so the honest worst case is that the mix
completes in the *next* period, within `W` (§5.3) + `C` = 2 525 669 ns of that
boundary — a total of `(P − φ) + W + C` after the wake, against a device
deadline of `P`, so **late by at most `W + C − φ` = 2 525 669 − φ ns, and never
by a whole period.** Two things sharpen that. The cap is what forces the
next-period route in the worst of the band: §1.6 lets a mid-period wake spend at
most `u · (P − φ)` before its boundary, which is below the mix cost for every
`φ > P − C/u` = 1 886 619 ns — a 35.0 % band of the phase space where finishing
in the wake period is arithmetically impossible rather than merely unlucky. And
**throughput is unaffected**: §1.9 delivers 580 µs in every aligned period soundd
is runnable throughout, against a mix cost of 203 175 ns, so what a bad phase
costs is one pipeline slot of *latency* — absorbed by the eight-period pipeline
with its five-period deferral floor (`audio-subsystem-spec.md` §4) — and not a
drained device. **Whether a device-driven client's grid should be anchored at its
stream rather than at admission is a design question this document does not
answer** (§10): it would be a second phase origin, and inventing one to rescue a
derivation is how the previous three attempts each began.

**The budget is an estimate, is declared as one, and prices soundd's own
cycle.** Under §1.8 a signalled client fills out of *its own* class's budget
with a mark on it, so this number covers soundd's consume, mix and submit and
nothing a client does — which is the property that makes the bound above
independent of client behaviour, hostile or merely slow. The only recorded
figure for soundd's CPU cost is roughly 7 % of a core — 203 175 ns of a
2 902 494 ns period — and `specs/reference/production-audio-baselines.md` §4.3
marks it **historical**: it described the pre-Stage-7a tree, where an idle
soundd mixed silence about 43 times a second, a behaviour deleted on 2026-07-31;
the standing recorded idle figure is a structural 0 %, and the reference bars
that 7 % from being quoted as a hardware number at all. So 580 µs is 2.855× the
cost of code that no longer runs, on a path that was never the mix path. It is a
placeholder with an order of magnitude and nothing more, and R8 is the chunk
that replaces it with a measurement — under §9.3's transform, not by writing
down what it reads.

**What the client's fill window is worth under the mark, derived.** A signalled
client is a fair-band thread on its own CPU, marked by soundd's wake and
therefore first inside its class. Its wait is the fair class's own first-dispatch
bound: `W_fair` = the smaller of the interference sum — `ceil(10 ms / P)` = 4
soundd budgets plus one dying-server budget = 3 320 000 ns — and `period −
budget` = 5 000 000 ns, so **3.32 ms**, plus 4 ms for each mark already ahead of
it in the class (§1.8): every mark carries the same 1 ms running-time window, so
that term is a constant per mark rather than a function of what the mark was
raised for. That is 1.14 device periods, so a cooperative client's fill lands
inside soundd's own wait when the mark queue is short and inside the deferral
window — three periods, `audio-subsystem-spec.md` §4 — when it is not. This is
the one place a marked client is a *different* process from its marker, which is
where the mark buys the marked half rather than nothing (§1.8). What the mark
abolishes is the shape commit 9c2fc4d measured before the lend existed: 93.3 ms
of starvation behind a fair storm and a 24-period, 70 ms gap. The fair class's
admitted floor is what makes that unreachable now, the mark is what keeps the
marked client at the front of it, and `old_park_kept_the_lend` (§5.5) is the
gate that holds the shipped behaviour the mark has to preserve.

The admission arithmetic for the shipped machine, on one CPU: 200 permille for
soundd, 100 for the dying server, 500 for the fair class — 800 of the 900
admissible, the 100 that remain are slack, and 100 more are the system reserve
(§1.3). The same set is admitted at every CPU count, because the reservation is
placed on one CPU and the rest of the machine is unaffected by it.

**`budget_ns` is an instrument-relative figure and the manifest says so.** It is
measured on QEMU under cross-arch TCG with an opt-level-0 kernel, an instrument
whose absolute CPU numbers `production-audio-baselines.md` declares
unrecoverable to metal, and it is then written into one `system.toml` for every
machine. That is honest only while the estate has one machine class; the metal
track re-measures it at its own gate under `specs/device-test-strategy.md`, and
until it does, a metal boot's soundd runs against a QEMU figure. Admission is
the only per-machine correction this design has, and admission can refuse but
never resize — which is why §9.3's branch structure exists rather than a hope
that the number travels.

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

> Release completes because the CPU that owns the victim serves it: every corpse
> on that CPU stands in one FIFO of *runnable* corpses, and the dying server
> delivers its admitted reservation to the head of it under §1.9 like any other
> entity. A corpse that waits is not in that queue and is owed nothing while it
> waits; the wait it is in carries a bound of its own — a device's declared
> timeout, or a lock holder's obligation under §1.8 — and the chain of them is
> finite because the sleep-lock set is. The retirer is given no deadline for the
> end of that chain and no assertion of its own about it. What it keeps is the
> fixed hops at its own end — the kick, its delivery, and the drain that puts
> the victim in the queue.

§8 is the derivation, and the principle it turns on is that the kill path
asserts *nothing* about a quantity a workload, a device or userland sets: what
the scheduler owes is already stated by §1.9 and measured by I15, and what the
unwind costs is reported rather than asserted.

### 5.3 Invariant I4 — real-time wake latency

I4's current bound is interrupt delivery, plus the longest preempt-off section,
plus the observation granularity, plus one dying chunk. The fourth term is
deleted (§3.2) and re-derived: what a real-time client waits for is not a
corpse's chunk but **the budget the other entities on its CPU may legitimately
spend before its own deadline arrives**.

> I4's bound is interrupt delivery, plus the longest preempt-off section, plus
> the observation granularity, plus `W`, where `W` is the **smaller** of two
> quantities: the interference sum — over the other entities on that CPU,
> `ceil(P / Pⱼ) × Bⱼ`, where `P` is the waking client's period, plus the system
> reserve's share of that period, `r × P` — and `period − budget` of the waking
> client's own reservation.

Both halves are derived and each is the tighter one in a different shape.

- *The interference sum* is what earliest-deadline dispatch can legitimately put
  in front of the client. An entity whose period is at least the client's
  contributes at most one budget, because a period boundary moves its deadline
  *later*; an entity with a **shorter** period replenishes inside the client's
  period and contributes one budget per replenishment, which is what the
  `ceil(P / Pⱼ)` factor counts. Nothing in this document orders the periods of
  two real-time clients on one CPU — §1.3's lower bound constrains the *fair*
  class only, and §7.1 admits any period in `[200 000, 10⁹]` — so the factor is
  the honest form and the earlier "at most one budget each" was true only of the
  shipped one-client machine. Its closed form is unit-coherent and needs no
  ordering assumption: `Σ ceil(P/Pⱼ)·Bⱼ ≤ P · Σ uⱼ + Σ Bⱼ` over the other
  entities. **The reserve is a term of this sum and was missing from it.** §1.2
  moves interrupt and pass time out of every entity's budget, so it appears in
  neither factor above while delaying the waking client exactly as interference
  does; `r × P` — the reserve's share of one of the client's own periods —
  restores it. It is the reserve as §1.3 holds it back, provisional with it, and
  it is written as a term rather than folded into a constant.
- *The guarantee half* comes from §1.9: a client continuously runnable from its
  wake receives its whole budget inside the period it woke in, so it cannot
  still be waiting `period − budget` after a wake that landed at a boundary. The
  argument is a counterfactual over an interval on which the client is runnable
  throughout, and the schedule up to its first dispatch does not depend on what
  it does after it, so the counterfactual is exact.

On soundd's CPU `ceil(P / 10 ms)` = 1 for both kernel entities, so the sum is
5 ms (the fair class) + 1 ms (the dying server) + 0.1 × 2 902 494 = 290 249 ns
(the reserve) = 6 290 249 ns, and the guarantee half is 2 322 494 ns; `W` is
2 322 494 ns. The missing term therefore moves no shipped number — the guarantee
half was and remains the smaller — and it bites only where `period − budget`
exceeds the sum. The bound is **larger** than the
`DYING_CHUNK_NS` = 1 ms term it replaces, and it is a bound in every workload
rather than in the one-corpse case; the term it replaces was smaller because it
counted one corpse, and `k` corpses made it false.

**Two shapes the bound does not cover as stated, named rather than left to be
discovered.**

- *A client that wakes with its budget already spent* is covered by neither
  half: the interference sum is over deadlines earlier than a deadline it has
  already passed, and the guarantee half presupposes a budget to deliver. Such a
  client is in the background tier (§1.5) until its next boundary, so its bound
  is `(next boundary − wake) + W`, at most `period + W` = 5 224 988 ns on
  soundd's CPU. It is a bound and not a hole, and a real-time client that
  regularly wakes exhausted is a client whose reservation is sized wrong —
  §1.5's demotion count is where that shows, before the latency does.
- *A client blocked on a resource* is **not covered by a bound, and I4 does not
  gate on it.** The clause the previous form stated over `W_block` is withdrawn
  rather than repaired: §1.8 no longer derives that wait, because its
  denominator is the holder's service rate and the shipped shape — a real-time
  thread on its own process's `ProcessData`, held by a fair-band sibling the mark
  pays nothing — puts that rate in the workload's hands. A gate here would have
  to carry both the mark population and the count of runnable fair processes in
  its own bound to be true of that shape, and this document declines to state a
  bound it would then have to repair a fourth time. **What replaces it is a
  report on both sides**: the kernel's blocked-on-lock counter (§1.8) and the
  harness's measurement of the same interval in §6.2's lock cells, printed
  beside I4 rather than compared against it. The obligation that keeps the wait
  short is unchanged and is the checkable half — a lock that declares no hold
  bound may not be taken where a real-time client can block on it.
  `old_park_kept_the_lend` still reds on the other side of the same mechanism,
  where a mark outlives the wait that justified it, and it reds under I9.

### 5.4 Invariant I14 — a retire reaches release, in the simulator

**I14 is a simulator invariant and has no kernel-side twin.** Every term it
composes — the unwind's own CPU, the phase, the zombie-freeing pass, and the
waits a corpse enters on the way — is a quantity the harness stages and reads
back off the run. That is why it can be checked at all: the kernel can see none
of them ahead of time, and an assertion it cannot evaluate is an assertion it
cannot own (§8's doctrine).

I14 keeps its clock — the wall clock, for the reason the current design already
records — and changes shape twice.

**The rate replaces the stretch factor.** An unwind's wall-clock cost is the
victim's own unwind time divided by the dying server's utilization, plus one
period for the phase the retire arrives in: with the server at 1 ms every 10 ms
the stretch is `period ÷ budget` = 10 exactly, so the modelled unwind —
`UNWIND_NS` = 4 ms (`toyos-sched/sim/src/vm.rs:81`), which is the length the sim
actually charges — is 40 ms of wall clock plus at most one 10 ms phase. The
unwind's length is the sim's own constant wherever this document prices one; a
quantum is what the *victim* may hold, not what an unwind costs. The elevenfold
factor is
deleted; it was the aged grant's delivery cadence — one chunk per
`DYING_AGE_NS + DYING_CHUNK_NS` — and the reservation does not have it.

**The `(1 + peers)` term goes away with the kernel deadline it modelled.** I14
becomes **head-relative**: it bounds the interval from the instant a corpse
reaches the head of its CPU's dying queue to its release, and the queue in front
of it is not inside the bound at all. That is the same reasoning §8 applies to
the retirer's deadline — the composition of `n` waits is the sum of `n` bounds,
and writing the sum down as a constant is what forces a term the workload sets
into a place nothing can read it — and there it ends in deletion, because the
kernel can read none of the terms. Here the sim can read them, so the bound
survives in head-relative form. **The end-to-end retire-to-release time is not a
sum this document states.** It is not FIFO order plus the head-relative bound
over the corpses ahead, for two reasons that were both wrong in the previous
form: `check_retires` asserts no ordering at all — its two halves are that a
killed task is never migrated and that each retire completes inside
`retire_latency_bound` — and §1.7's rejoin rule means a corpse that parks
returns to the *tail*, so it is overtaken by whatever arrived meanwhile and its
release is a function of how often it parks and how deep the queue was each
time. The sim prints what it measured, per corpse and end to end; nothing
derives a constant from either, and liveness does not need one (§5.2 rests on
the chain being finite).

> I14's bound, per corpse: from reaching the head of its CPU's dying queue,
> release completes within the corpse's remaining unwind CPU time ÷ the dying
> server's utilization, plus one dying-server period of phase, plus the pass
> that frees the zombie.

**A wait does not inflate that interval, because a waiting corpse is not in the
queue** (§1.7): it leaves the queue when it parks and rejoins at the tail when
it wakes, so a park *ends* the head-relative interval and a fresh one begins
when the corpse next reaches the head. The waits are therefore not a term of
this bound; they are separate intervals — a device wait carries that device's
declared timeout, a lock wait carries the report of §1.8 and no bound — and the
sim reports them beside I14 rather than folding them into it. A release that is late by a device timeout is a fact about the
device, and a bound that absorbed it would be a bound about nothing.

I14 is not a special case of §1.9; it is a consequence of it, and it stays a
separate check because it measures the composition — the message hop, the rate,
and the zombie-freeing pass — rather than one entity's service.

### 5.5 The negative gates

The simulator's negative gates and its controls are law. Each is stated below as
surviving verbatim, re-derived, replaced by a strictly stronger gate, or —
where the machinery it certified is deleted by this document — deleted with it
and *said to be*. **No surviving gate weakens, none loses the invariant that
certified it, and no invariant is left without a certifier.** Each added gate
names two things: what reds — a roster invariant, or the staged design's own
tripwire — and the cell of §6.2 it reds on. A must-red claim that names neither
is a claim nobody can check, and a gate whose subject this document deleted is
not kept as decoration.

| gate | disposition |
|---|---|
| `old_steal_port` | **survives verbatim.** The old steal-and-scan algorithm is a placement and ownership defect; reservations touch neither. |
| `old_commit_before_pass` | **survives verbatim.** The blocking protocol is untouched. |
| `old_preemptible_window` | **survives verbatim.** The registration window's preempt-off requirement is untouched. |
| `old_migrate_kept_the_corpse` | **survives verbatim.** A corpse handed to another CPU is a corpse taken away from the dying server that was admitted to serve it, which is if anything a sharper statement of the same break. |
| `old_rt_starved_the_corpse` | **replaced by a strictly stronger gate, and the arithmetic of "stronger" is below.** Its escape hatch stages "the real-time band outranks every corpse", which is one point of the design space this document deletes. Its replacement stages "the dying server is dispatched only when no real-time client is runnable" — the same break expressed against entities — on the same scenario shape, so it keeps the old gate's quantifier: **it must red under I14 on every seed**, which is the certification the old gate carried and the only one re-derived I14's rate term has. It **also** reds under I15, and on two shapes the old gate could not see. |
| `old_park_kept_the_lend` | **re-derived, and it keeps I9.** The break is unchanged — a park that keeps a lapsed window — but the window is now §1.8's mark, so the gate stages a park that keeps a mark after the wait that raised it ended. What catches it is **I9**, the boost-window invariant, re-derived at R4 into the mark's terms (§6.1): one raise buys precedence until one of §1.8's three end conditions and no further. It is I9's only certifier (`toyos-sched/sim/tests/scenarios.rs`'s register), so moving the catch to I5 would have retired that certification — and I5 could not have taken it, because §2 scopes I5 to the service delivered while no mark was being honoured, which excludes precisely the service a stale mark buys. This gate is also what holds the shipped pipe-lend behaviour commit `9c2fc4d` measured (§4.1). It needs `Op::Acquire`/`Op::Release` to be stageable at all (§3.2), which is R4's prerequisite and not a licence to land R4 without it. |
| `fair_share_per_thread` | **survives verbatim.** It is I5's negative gate and per-process fairness is inside the fair class, which reservations do not touch. What changes is I5's *scope*, not this gate: §2 puts §1.8's marked half inside what I5 measures, and `old_uncapped_mark` is the gate for that half. |
| `fair_double_charge` | **survives verbatim.** |
| `fair_identity_within_share` | **survives verbatim**, and gains a second job: it is the gate that holds §2's seam, since an intra-fair policy replacement that regressed thread identity would red here. |
| `overlong_pass` | **survives verbatim.** The pass budget is a property of a pass, not of a band. |
| `old_commit_fused` (control) | **survives verbatim**, and must still come back clean. |
| `fair_identity_tiebreak` (control) | **survives verbatim**, and must still come back clean. |
| **`old_unbounded_rt_precedence`** (new) | Stages the abolished rule: a real-time client is dispatched whenever it is runnable, ignoring budget and deadline. Must red under I15, on every seed, in the cell (real-time load = one client that attempts to exceed its reservation; corpses ≥ 1; band continuously occupied): the client spends past its budget, the dying server receives less than 1 ms in the period, and the deficit is the whole overrun. |
| **`old_aged_grant`** (new) | Stages *this document's predecessor* — per-corpse age stamps, a one-chunk grant ahead of the real-time band, and a restamp on every re-entry. Must red under I15 in two cells, and the arithmetic of each is below. The design being replaced becomes the gate that proves the replacement measures something. |
| **`old_arm_time_snapshot`** (new) | Stages the superseded §8: the retirer reads the victim CPU's queue depth when it arms and computes a wall-clock deadline from it. What reds is the staged design's **own tripwire**, not a roster invariant — that is the point of it, since the schedule breaks no rule this document states. The cell is (retirer concurrency = `k` ≥ 4 inside one drain window; corpses ≥ 4; real-time load = one client that spends its whole budget), with the dying server honouring its reservation exactly throughout, so the red reads "a deadline expired while every reservation was met". The review's probe `probe_arm_time_depth_is_blind_to_in_flight_siblings` is this gate's seed: it already demonstrates the blind read on four victims, and what the gate adds is the deadline the read was feeding and the release time it fails against. |
| **`old_underdelivered_dying_server`** (new) | Stages a scheduler that delivers the dying server less than its reservation while it has runnable corpses. Must red under **I15** on every seed, in every cell with corpses ≥ 1. Its subject survives this revision's deletions intact: I15 is what says the dying server is owed its budget, and this is the gate that proves I15 has teeth on the one entity nobody outside the kernel can observe. |
| **`old_uncapped_mark`** (new) | Stages §1.8's mark without its cap or its window: a marked thread is dispatched ahead of unmarked threads for as long as it holds the resource, which is the shipped lend's unpriced promotion in the mark's clothes. Must red under **I5**, the per-process fairness invariant `fair_share_per_thread` already certifies, in the cell (marks = `k` concurrent in one class; fair load = a storm), where the marking processes take the whole class and the unmarked ones fall below their share. |
| **`many_victims_many_retirers_slow_device`** (new control) | `m` victims, `k` concurrent retirers, and an in-unwind device wait at its own timeout, on a CPU whose real-time client spends its whole budget every period — the cell (corpses = 9; retirer concurrency = `k`; in-unwind device wait = one at its declared timeout; real-time load = one client that spends its whole budget). Must come back **clean**, and now nothing in the kernel could make it dirty: this is the schedule every previous tripwire panicked on, no assertion in this document is stated over a device's wait, and a corpse inside one is not in the queue the reservation serves (§1.7). |

**Three gates this document's previous forms proposed are deleted with the
machinery they certified, and none leaves an invariant uncovered.**
`old_stalled_head_corpse` staged a served head corpse that bumped no progress
marker: there is no marker (§8), so there is nothing to stage, and the invariant
it named — I14 — keeps its certifier in `old_rt_starved_the_corpse`'s
replacement, which must red under I14 on every seed.
`old_unaccounted_wake_grant` staged a grant that charged nobody: under §1.8
*nothing* is charged, deliberately, so the break it staged is now the design, and
the direction it guarded — precedence without a bound — is guarded instead by
`old_uncapped_mark`. `old_mark_dropped_under_a_waiter` staged a mark ending
under a live waiter, and it goes for two reasons: the clause it would have red
under is withdrawn (§5.3), and §1.8 *itself* ends a mark when the marked thread
blocks,
so a faithful implementation produces the staged break and the gate cannot tell
the design from the defect. Its direction is now measured — the blocked-on-lock
report in the kernel and the same interval in §6.2's lock cells. (It is the
same subject the previous revision carried as `old_donation_not_renewed`, so
what leaves here is one gate under two names and not two.) None of the three was
landed, so the deletions cost text and no coverage; keeping a gate whose subject
is gone would have been a claim the harness cannot check.

**Which cell each must-red claim reds on, with the arithmetic.** The continuity
dimension's recurrence interval (§6.2) decides two of these, which is why it
exists:

- `old_rt_starved_the_corpse`'s replacement reds where the dying server receives
  nothing: band continuously occupied (service 0), and band briefly empty with
  the gap at one interrupt delivery at any recurrence (the gap is microseconds
  against a 1 ms budget). It does **not** red at (gap = one execution chunk,
  `RUN_CHUNK_NS` = 1 ms, recurring once per dying-server period), because 1 ms
  per 10 ms is exactly the reservation — so that cell is named as a pass, not
  claimed as a red. At recurrence intervals sparser than one gap per
  dying-server period some period receives nothing and it reds again.
- `old_aged_grant` reds (a) with the band continuously occupied, where the grant
  delivers `DYING_CHUNK_NS` per `DYING_AGE_NS + DYING_CHUNK_NS` = 1 ms per 11 ms
  = 909 091 ns in a 10 ms period against 1 000 000 owed, a 90 909 ns deficit per
  period that I15's cumulative comparison accumulates past `G` inside three
  periods; and (b) with the band briefly empty at one interrupt delivery
  recurring inside the age window, where every gap restamps the corpse and its
  measured rate has no lower bound at all. The many-corpse values of §6.2 (9,
  11, 16) are where (a) is sharpest, because the grant is per corpse and the
  reservation is not.

A gate that stopped being able to red would be this document weakening the
harness to admit itself, which is the one thing the negative-gate rule forbids.
That is why "strictly stronger" here means *the same invariant on the same
quantifier, plus more* — a replacement that moved a catch from I14 to I15 and
called the move an improvement would have retired the only certification the
rate term has.

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

**§7.2a's amendment stands, and two names inside it do not.** The contradiction
it recorded, between a killed task being dispatched and the law saying it never
is, was resolved in the law and is not reopened here. What is reopened is its
arithmetic: the sentence citing `(1 + peers) × UNWIND_NS ×
rt_deferral_stretch()` carries two deleted names (§3.2) and a term §5.4 no
longer has, so it is restated against the rate — the unwind's own CPU time
divided by the dying server's utilization, head-relative — in the same pass that
lands R7.

**§24's open risk about the audio wake latency to read on the next boot** is
what §4.2 turns into an acceptance criterion. The risk is not closed by this
document — it is closed by the measurement §4.2 requires.

**§22's failure-mode table has two rows that state superseded law, and §3.3
dispositions both** (`:3192`'s aged-grant dispatch rule and `:3194`'s discharged
`GIVE_UP` deadline with its filed `peers` residual). They are named there rather
than here because they are deletions at a site, not reconciliations of an
argument: the first becomes §5.1's entity sentence and the second becomes §8.3's
fixed-hop tripwire, and the defect the residual was filed as closes with the
deadline that carried it.

**§9's lock table is where §1.8's per-lock hold bounds land.** This document
states the obligation — every sleep lock in the fixed set declares a bound on
how long it may be held, and a lock that declares none may not be taken on a
path where a real-time client can block on it — and the sibling states the
bounds, beside the four locks it already names. Writing them here would put a
lock's contract in a scheduling document and leave the lock's own law silent
about it (§10).

---

## 6. The simulator

### 6.1 One new invariant

> **I15 — a runnable entity is never underserved.** For every entity, over every
> one of *its own periods* in which it was continuously runnable, the CPU time
> it received is at least its budget, measured on the wall clock.

That is §1.9's sentence, word for word, which is the point: the law and the
instrument are one predicate and neither can drift from the other by a rewrite.

**It is checked cumulatively, with one tolerance and not one per period.** Over
a stretch in which an entity was continuously runnable, the service it has
received must be at least the sum of the budgets the completed periods in that
stretch were *replenished to* — `budget`, less any overrun §1.6 charged back —
minus one delivery granularity `G` (§1.9, `MAX_PASS_NS` = 200 000 ns). The
per-period form would let a scheduler spend `G` in every period for ever and
stay green — 90 909 ns per period is what the aged grant's rate costs, and it
would have hidden inside a 200 000 ns per-period allowance; the cumulative form
accumulates it and reds inside three periods. The check runs after every step,
like the other global walks, and on the wall clock. Its violation message names
the entity, the stretch, the budget owed, the service delivered and the deficit,
so that a red says which side of the machine lost rather than that a bound was
exceeded.

I15 is the only new invariant. I4 and I14 are re-derived (§5.3, §5.4) and stay.
The ownership and accounting invariants are untouched. **Three are amended and
this document says so rather than listing them as untouched**: the timer
invariant gains §1.11's arming list (the running entity's exhaustion instant and
the boundaries that can change the winner join `quantum_end` and the parked
deadlines); the boost-window invariant **I9** is re-derived at R4 into the mark's
terms
— a window that ends at one of §1.8's three conditions, honoured only inside the
marked thread's own class, and charged to that class rather than to whoever
raised it; and **I5 gains the marked half in its scope** (§2), comparing
processes over the class's unmarked service and reporting the marked service
per process beside it, so that a mark which farms is visible to the invariant
that measures farming.

**The liveness half is required too**, under the same rule the other measured
invariants follow: a run in which no entity was ever continuously runnable for a
whole period proves nothing, so the harness reports the fraction of periods I15
actually compared and gates on that number as well as on pass or fail. A change
that closes I15's window is as loud as one that violates it.

**The entity I15 will least often have a window on is the audio client**, and
saying so is the honest form of the fraction above: soundd mixes for a fraction
of its period and blocks, so it is continuously runnable for a whole period only
when it is already late, and I15 may compare nothing on it across a whole run.
That is not a hole in §1.9 — §4.1 and §5.3 derive soundd's bound as a
counterfactual over the interval it *is* runnable on, and §5.3 says why the
counterfactual is exact — but it does mean I15 is not soundd's instrument. I4's
wake latency and gate A's measured audio (§4.2) are, and a reading of the
liveness fraction that expected soundd to fill it would be reading the wrong
number.

### 6.2 The standing scenario matrix

The shapes that broke the three previous attempts become the standing scenario
set, and they are run in both directions: every scenario asserts both that
real-time work got its reservation and that the dying server got its.

| dimension | values |
|---|---|
| corpses on one CPU | 0, 1, 2, 3, 9, 11, 16 |
| real-time load | none; one client inside its reservation; one client that spends its whole budget every period; one client that attempts to exceed it (which §1.5 demotes) |
| real-time band continuity | continuously occupied; briefly empty with the gap at one interrupt delivery, at one execution chunk, and at zero |
| the recurrence interval of those gaps | one gap per real-time period; one per dying-server period; one per three dying-server periods |
| fair load | idle; one thread; a storm |
| lock holding | none; a fair-band thread holds a lock a real-time client blocks on — same CPU and other CPU, **and a thread of the waiter's own process or of another**; a two-link chain |
| marks | none; one; `k` concurrent in one class; one kept after its wait ended; one dropped while its waiter still waits |
| retirer concurrency | one retirer; `k` retirers aiming victims at one CPU inside one drain window |
| in-unwind device wait | none; one at its declared timeout |

The values that are not arbitrary:

- **9 corpses** is what the superseded tripwire priced as its workload term.
- **11 corpses** is where the aged grant's rotation closed on itself, because
  eleven one-millisecond chunks fill an age window plus a chunk. It is the shape
  that must red under `old_aged_grant` and pass under the reservation.
- **A zero-length gap** in the real-time band is the shape under which the aged
  grant's measured service rate had no lower bound at all.
- **The recurrence interval** is what decides whether a gap-shaped break is
  visible at all, and its absence is what made two of §5.5's must-red claims
  uncheckable in this document's first form: a 1 ms gap once per dying-server
  period delivers exactly the reservation and reds nothing, while the same gap
  once per three periods leaves two periods empty and reds. A dimension that
  fixes a gap's length but not its cadence is a dimension that decides half a
  question.
- **The lock and mark dimensions** exist because §1.8 is the one mechanism in
  this document whose failures are invisible to every other row: a blocked
  waiter is not continuously runnable, so I15 opens no window on it, and the
  wait it suffers is charged to nobody. Those cells are therefore where the
  blocked-wait *report* is read (§1.8, §5.3) rather than where a bound is gated,
  and the same-process value is in the dimension because it is the shape the
  shipped tree produces and the one where a mark buys nothing. They are also the
  two dimensions the
  sim's `Op` vocabulary cannot yet express (§3.2). **`k` concurrent marks** is
  the value the previous form of §1.8 had no bound for: one holder made every
  stated wait true and a second made it false, so a dimension with only "live,
  lapsed, kept" measured the mechanism at exactly the multiplicity where it
  worked.
- **The retirer-concurrency and device-wait dimensions** exist because §5.5's
  `old_arm_time_snapshot` and its must-stay-clean control are both stated over
  coordinates the other five dimensions do not contain: `k` retirers inside one
  drain window is what makes an arm-time depth read blind, and an in-unwind
  device wait at its declared timeout is the shape every superseded tripwire
  panicked on. A gate that reds on a schedule the matrix cannot express is a
  gate nobody can run.

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
- **`budget_ns` below `G` = `MAX_PASS_NS` = 200 000 ns is refused by name**, at
  the parser and again at admission. §1.9's granularity is the amount an entity
  can overrun by before the pass that stops it lands, and §1.6 charges that
  overrun back against the next refill: a budget smaller than `G` makes
  `budget − overrun` negative, which is a quantity no rule in this document
  defines and both available readings falsify — clamping at zero forgives a
  standing gift the charge-back exists to abolish, and carrying the debt
  contradicts "paid in full at the first boundary". This is §7.1's own
  period-floor argument applied to the other coordinate, where it was always
  equally true: a budget below the granularity the guarantee is delivered at is
  a reservation the machine cannot honour whatever the arithmetic says.
- **`period_ns` outside `[200_000, 1_000_000_000]` is refused**, and the two
  ends are derived rather than round. Below `MAX_PASS_NS` = 200 000 ns
  (`toyos-sched/src/cpu.rs:893`) a period is shorter than the granularity the
  guarantee is delivered at (§1.9's `G`), so the reservation is one the machine
  cannot honour whatever the arithmetic says. Above one second the reservation
  stops meaning anything a real-time client wants — §5.3's latency term counts
  at least *one budget of each other entity*, and a legal 200-permille client
  with a 3 600 s period would put a 720-second budget inside it — and the
  product
  `1000 × budget_ns` stops fitting comfortably inside 64 bits. Both refusals are
  by name, like the four above.
- **The admission arithmetic is checked and refuses by name on overflow**, in
  init's check and in the build gate, which share the formula. With the period
  ceiling above, `1000 × budget_ns ≤ 10¹²` and nothing can wrap; the check stays
  because the alternative is a manifest with `budget_ns = period_ns = 2×10¹⁶` —
  legal under every other rule here, a true utilization of 1000 permille — being
  admitted at 78 permille by a wrapped multiply, which would make §1.3's
  "overcommit is unrepresentable" false at the arithmetic level rather than at
  the policy one.
- An unknown key inside `reservation` is refused, exactly as an unknown `syscap`
  name already is.

### 7.2 The endowment-time check

`/bin/init` builds every program's authority before spawning it, and the
admission check runs there, in that order: the reservation is admitted against
the CPU it will be placed on **before** the program is started, and a program
whose reservation cannot be admitted is not started at all.

The check is arithmetic on numbers init already holds — the manifest's
reservations, the two kernel-owned constants of §1.3 and the system reserve it
subtracts first — so a machine whose manifest overcommits fails at the first
boot of that manifest, in the same place and with the same shape as a manifest
naming a right that does not exist.

### 7.3 Refusal wording

A refusal names five things: the program, the reservation it asked for, the CPU,
what is already admitted there, and what remains. It reads as arithmetic,
because arithmetic is what the reader has to check:

> `soundd: reservation 580000/2902494 ns is 200 permille; cpu0 has 300 permille
> for real-time work and 250 are already admitted (compositor 250); 50 remain.
> Refused.`

The remainder is named and not left to the reader's subtraction, because the
rule above says five things and a message that names four is a message whose
last line the reader has to compute — which is exactly the arithmetic a refusal
exists to hand over.

**At endowment the refusal is fatal to the program's start, and is not a
degraded start**: a server that runs without the reservation it was written
against is a server that will miss its deadlines quietly, which is the failure
mode this whole design exists to make impossible.

**At a move (§1.10) the same five things are reported and nothing is fatal**,
because nothing was lost: the thread keeps the reservation it holds on the CPU
it is on, and the placement policy that asked is told which ledger said no. The
two readings of one wording are the two things a refusal can mean — a program
that cannot start, and a move that does not happen — and they are distinguished
by where the refusal was raised rather than by two message formats.

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

## 8. The retirer's wait, and what the kill path may assert

**The kill path asserts nothing of its own.** That sentence is the whole of this
section; the rest is why it is stronger than the three designs that asserted
something, and what takes the assertions' place.

**The doctrine, which every later change to this document is measured
against:**

> A kernel assertion whose falsity is a panic may only be about what its own CPU
> can observe locally and what no workload can scale: whether the scheduler
> delivered the service its own rules promised. Latency, progress and completion
> are *composed* quantities — device time, lock chains and userland-set work
> sizes enter them — so they are measured, reported and gated in the harness and
> the simulator, and never panic the kernel. An estimate never panics.

**And the same rule one level down, because that is where the third pass found
it:**

> **No hard bound at any level — a kernel assertion, a harness gate, or a
> derived constant — may be stated over a quantity a workload sets, unless the
> bound carries that term explicitly.** Everything else is measured and
> reported. A number does not stop being workload-set by being written down as a
> constant, and a gate is not safer than a panic for being a red.

Three adversarial passes over this document found the same defect seven times,
and each instance had this shape. A retirer's wall-clock deadline over `k`
retirers and `m` transfers. A sliding window. A head-relative bound over a
device wait. A queue-occupancy condition over a parked corpse. A progress
cadence over the dirty pages of one file. Then, inside the repairs themselves: a
must-not-red bound on a blocked wait whose denominator the runnable process
count sets, and a reserve constant derived from a pass rate the admitted set
multiplies. The first five moved the term out of the kernel; the last two are
what proves that moving it was not enough. The doctrine removes the class, and
the deletions below and in §1.3 and §1.8 are what applying it costs — including
two mechanisms the previous round's rulings had added a day earlier.

### 8.1 The superseded design, which is what one gate stages

The form this replaces had the retirer read the depth of its victim's CPU's
unwind queue when it armed and compute a wall-clock deadline from that depth and
the dying server's declared rate. That read precedes every arrival — a retire is
a message, and a victim joins the FIFO only when its CPU drains it — so `m`
killers aiming victims at one CPU inside one drain window all read approximately
zero while all `m` victims are in flight. That is what `old_arm_time_snapshot`
(§5.5) stages and what `probe_arm_time_depth_is_blind_to_in_flight_siblings`
seeds. The rest of the story — invariant 2's exclusivity objection, the device
wait inside `close_all`, and both withdrawals — is in
`specs/issues/kernel/retire-tripwire-is-not-queue-shaped.md`, which is the file
that closes on it and the one place it belongs.

### 8.2 The dying server is an entity, and that is the whole service guarantee

**The previous form of this section put two assertions on the victim's CPU, and
both are deleted.** They were called locally checkable, and their conditions
were not:

- *"The dying server delivered its budget over each period in which its queue
  was non-empty"* is `§1.9` with the wrong condition. §1.9 conditions on
  *continuous runnability*; a queue-occupancy condition and a runnability
  condition are the same sentence only under §1.7's rule that the queue **is**
  the runnable set — which this document now states, in the place that owns it.
  With that rule the assertion says of one entity what I15 already says of every
  entity, over the periods the assertion could have been true of; the periods it
  could have been true of and I15 opens no window on are the ones a parked corpse
  creates, where the queue is non-empty and the server is not runnable — which is
  exactly the half of the assertion that was reachable from legal userland and
  the half it panicked on. Its deletion therefore loses nothing: **I15 covers the
  dying server correctly, and no `k` changes what one entity is owed per
  period.**
- *"A served head corpse bumps a progress marker"* was a panic over a cadence
  the process's own size sets — a flush's dirty pages, a region's freed pages,
  both chosen by userland. The 2026-08-17 review measured **14.4 ms** between
  two legal bumps on a host port of the kernel's own `pmm` and ticket lock, so
  the tripwire panics on input that broke no rule; and threading the marker
  deeper is not a repair, because it puts a scheduling obligation into `vfs.rs`,
  `toyos-fat32` and drop glue while leaving the cadence with the workload.

**What replaces them is not a third assertion. It is the guarantee that was
always the real one**, plus a report:

- **The service guarantee is §1.9's, and the instrument is I15.** The dying
  server holds an admitted reservation; over every period in which it is
  continuously runnable it receives its budget; a corpse in a wait is not in its
  queue and is owed nothing while it waits (§1.7). That is local, it is what the
  scheduler itself promised, and no workload scales it.
- **Progress is observability, not law.** The CPU counts what it can cheaply
  see: how long the corpse at the head has been at the head, how many corpses
  are queued behind it, and how much service the dying server delivered over
  that stretch. When the head's tenure exceeds a **derived expectation** — the
  modelled unwind at the modelled rate, `UNWIND_NS` = 4 ms
  (`toyos-sched/sim/src/vm.rs:81`) ÷ 0.1 plus one 10 ms period ≈ 50 ms — the
  kernel emits one log line naming the corpse, its tenure, the queue depth and
  the service delivered, and increments a counter. Loud, attributable, and
  **never fatal**: the expectation is an estimate about a quantity userland
  sets, which is precisely why it reports instead of panicking.
- **The waits inside an unwind are somebody else's obligation, and the chain
  terminates.** A corpse waiting on a lock waits for its holder, and every
  holder is either alive — served by ordinary reservation law and, while a
  corpse waits on it, marked under §1.8 — or itself a corpse in another CPU's
  dying server. A corpse inside a device wait waits at most that device's own
  declared timeout. The sleep-lock set is fixed and ordered
  (`{ProcessData, VFS, VOLUMES, XHCI}`, `completion-architecture-spec.md` §9),
  so no cycle exists and no chain is longer than four links; every link is
  finite; the sum of finitely many finite waits is finite. That is the whole of
  the liveness argument, and it contains no constant a workload can grow — and
  now no assertion is stated over it either.

**Four decisions the deleted assertions forced on their implementer disappear
with them**: how finely the marker must be threaded, how a library crate bumps
one, what declares a device wait and at what granularity, and whether R7 may
land before the completion spec's C7 turns an in-unwind USB spin into a park.
None of the four has an answer in this document, because none of the four is
asked any more.

### 8.3 What the retirer keeps

The retirer parks uncancellably for its victim's release, as it does today, and
it keeps **one fixed-hop tripwire** covering only its own end of the protocol:
the kill bit and the kick, the kick's delivery, and the drain that puts the
victim into the FIFO. Its terms are the cross-CPU hop, one `G` (§1.9,
`MAX_PASS_NS` = 200 000 ns) for the pass that acts on it, and one pass prologue
— no queue depth, no unwind length, no device wait, and nothing at all that
happens after the victim is in the queue. Past that point the retirer has no
deadline: what says the wait ends is the dying server's admitted reservation and
the finite chain of waits above, and neither is a thing the retirer can time.

**The prologue still dominates that constant and still says so.** A scheduler
pass opens by draining device interrupts and that drain can reach a blocking USB
path with a two-second bound, so the shipped fixed-hop constant is
`USB_TIMEOUT_NS` = 2 s plus microseconds — the filed defect
`specs/issues/kernel/scheduler-pass-blocks-in-xhci.md`, not a property of this
wait. When it closes, this constant becomes microseconds, and it is the only
number in the retire path that the defect still inflates: the twenty measured
prologues that made the old end-to-end derivation hopeless were twenty because
the old bound spanned the whole chain. This one spans one hop.

**The duration taxonomy is not asked for anything new.** The previous form
needed a panicking kind that took a citation instead of an absurdity, because it
panicked on a derived wall-clock magnitude. What survives here is one fixed-hop
`Tripwire` whose expiry is absurd — a kick that never arrives — and a report
that is not a duration kind at all. `kernel/src/time.rs` is unchanged by this
design, and §10 records the withdrawal.

**One thing this cannot see, stated rather than left implied.** A CPU that has
stopped taking interrupts entirely fails every deadline on it, and nothing
evaluated on that CPU can fire — not a report, and not the one tripwire that is
left. That is not a scheduling failure and its detector is not the retirer's
wait.

### 8.4 What the sim prices, and what it does not

I14 (§5.4) is the head-relative bound and the sim reads its terms off the run,
which it can and the kernel cannot. With the dying server at 1 ms every 10 ms
and the sim's modelled unwind at `UNWIND_NS` = 4 ms
(`toyos-sched/sim/src/vm.rs:81`), one unwind costs `4 ms ÷ 0.1` = **40 ms of
wall clock**, and a victim behind `n` others that never park is
`(1 + n) × 40 ms` plus **one** 10 ms phase term — the phase is paid once, when
the retire arrives, because every later unwind starts inside a period stream
that is already running. That composition is what the sim prints, not a bound it
asserts (§5.4: a corpse that parks rejoins at the tail). The rate comparison is
the part worth keeping: the superseded design delivered one chunk per
`DYING_AGE_NS + DYING_CHUNK_NS` = 11 ms, an 11× stretch, against this design's
`period ÷ budget` = 10× — the same modelled unwind costs 44 ms there and 40 ms
here. They differ by 10 %, in the direction that says the reservation delivers
an unwind *faster* than the design it replaces.

**The unwind's own length is an estimate and stays declared as one.**
`UNWIND_NS` is a stand-in for handle closes and a teardown, derived in the sim
from what I4's widest bound has to be able to show rather than from a
measurement of one; the reservation multiplies it, it does not measure it, and
**nothing the kernel
asserts depends on it** — which is the difference between this section and the
three designs before it. The sim may multiply an estimate because the sim reds a
test; the kernel may not, because the kernel panics a machine.

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
work. They are also their own branch: this document lands with the work that
happened to precede it, and R1 starts fresh — chronology is not architecture.

| chunk | content | size |
|---|---|---|
| **R1** | The reservation type, the per-CPU admission ledger, §1.3's policy constants — the fair class's own `(budget, period)` and the system reserve among them — and the checked admission arithmetic with its overflow refusal, run against the admissible capacity rather than 1000 permille. Host code and tests only, no dispatch change and no behaviour change. | small |
| **R2** | The dying server: the entity, its reservation, its queue, demotion to the background tier and replenishment, **with §1.11's timer discipline and preemption predicate**, because an exhaustion nothing notices is not one. Deletes §3.1's aging cluster and dispositions its four crate gates in the same chunk, since a tree with both is a tree with two answers. Re-derives I14 head-relative, re-stages `old_rt_starved_the_corpse` — which is the only gate that certifies I14's rate term, so it lands with the term it certifies — and lands `old_aged_grant`. | large |
| **R3** | Real-time clients become reservation clients: earliest-deadline dispatch among entities with budget, the total tie-break, demotion at exhaustion, replenishment at the boundary with the overrun charged back, and the background tier's work-conserving order — the second half of §1.11 lands here with the entities that need it. Re-derives I4 with its multiplicity factor and lands `old_unbounded_rt_precedence`. Deletes `RunningTask::serves_rt_band` with its last caller, the sim's I4 predicate (§3.2). Amends `scheduler-core-spec.md` §5's real-time wake placement in the same chunk (§10). | large |
| **R4** | §1.8's urgency mark: the two events that set one, the total order, the marked half, the one running-time window with the kernel constant it needs, the same-process rule, and the mark's transitive walk along the sleep-lock chain. Lands the blocked-on-lock counter and its report, and asks §1.8's static question of `ProcessData` rather than assuming it. **Prerequisite inside this chunk**: `Op::Acquire`/`Op::Release` in the sim's workload vocabulary, without which the waiter/holder trigger cannot be staged, `old_park_kept_the_lend` does not exist as a gate and the lock cells have nothing to measure. Re-derives the boost-window invariant I9 and lands `old_uncapped_mark`. Amends `scheduler-core-spec.md` §3's pipe-lend paragraph and `audio-subsystem-spec.md` §4 to the mechanism the tree then has. | medium |
| **R5** | The manifest row, the parser's refusals (the period bounds and §7.1's budget floor among them), init's endowment-time check against the admissible capacity, the refusal wording with its five things, and the build gate. | medium |
| **R6** | I15 in its cumulative form and the §6.2 scenario matrix with every dimension — recurrence interval, lock holding, marks, retirer concurrency and the in-unwind device wait — with the liveness fraction reported. | medium |
| **R7** | §8 at the sites: the head-tenure counters and the report that replaces the two deleted assertions, the retirer's fixed-hop tripwire in place of `GIVE_UP`'s end-to-end form, and `old_arm_time_snapshot` and `old_underdelivered_dying_server` plus the `many_victims_many_retirers_slow_device` control. Closes the queue-shaped-tripwire defect — by deleting the wait's end-to-end deadline and putting nothing in its place — and amends `scheduler-core-spec.md` invariant 7 and the completion spec's §7.2a, §7.3 and §22 to their reservation forms. | medium |
| **R8** | Gate A's thorough tier against the recorded sample, both audio configurations at both CPU counts, soundd's budget re-measured and written back under §9.3's transform, and §1.3's system reserve measured on the same runs. | measurement |

R2 and R3 are the two large chunks and they are deliberately separate: the
dying server proves the mechanism against work nobody can observe from userland,
and only then does the audio client's guarantee depend on it. R7 is a medium
chunk that now *removes* the design's only panic instead of implementing two,
and it keeps that size because the counters, the report and the site amendments
are the work. R8 is what decides whether the design ships, and it is a
measurement rather than a review.

### 9.3 R8's transform, and every branch it can take

**The rule, written once so that R8 has nothing to invent:**

> `budget_ns := 2 × the worst per-period budget spend measured`, rounded up to a
> round number.

Each half is derived:

- **What is measured is soundd's own per-period budget spend.** Under §1.8 a
  signalled client's fill is charged to the client's class and not to soundd's
  budget, so what has to fit inside `budget_ns` is soundd's consume, mix and
  submit — and the number is therefore a property of soundd's code and the
  stream count rather than of what a client does with its window. The worst is
  taken across gate A's four configurations *and* across a multi-client,
  resampling measurement run made for this purpose, because the mix cost grows
  with streams even when no client misbehaves. That run is a measurement and not
  a fifth gate-A configuration: it neither joins `tests/audio-baseline.toml` nor
  gets a recorded sample, because adding a config to the gate is a change to the
  acceptance instrument and this is a change to one number.
- **The factor is 2, and it is the estate's own factor.**
  `tests/audio-baseline.toml` derives its per-run ceilings as "2× the observed
  maximum, rounded up to a round number", for a reason that applies here
  unchanged: these are max-of-window order statistics with heavy right tails,
  and at n = 30 the observed maximum sits near the 97th percentile, so a budget
  *at* the measured maximum would exhaust on the order of once every thirty
  runs. Reusing an argued factor is cheaper and more honest than inventing a
  second one, and "replaces the number with what it reads" — which prices no
  headroom at all — is deleted as text.

**The branches, all three of them, decided here rather than discovered at R8.**
The ceiling is 300 permille of the period = 870 748.2 ns (§1.3's real-time
ceiling, under the system reserve), so the written-back budget `B′` falls into
exactly one of:

- **`B′ ≤ 580 000`.** The estimate was generous. The budget falls, the slack on
  that CPU grows, and R8 lands with gate A green.
- **`580 000 < B′ ≤ 870 748`.** The budget rises. Admission still passes on the
  shipped one-CPU set, and gate A's distributional comparison is what decides:
  the tone client's wake-lateness distribution either moved or it did not. **A
  red here is escalated to the owner as a design question**, with the measured
  numbers, and is never absorbed by re-recording the baseline or by trimming the
  budget until the gate goes quiet.
- **`B′ > 870 748`.** Admission refuses and the machine would boot without
  audio. **The ceiling does not bend, and neither does the fair floor or the
  system reserve**: they are what makes every other guarantee in this document
  true, and a design that moves them to fit a measurement has stopped being an
  admission test. `B′` above the ceiling means a measured spend above 435 374 ns
  — 2.14× the historical 203 175 ns figure for a whole period of soundd's cost,
  and since §1.8 no longer charges a client's fill to that budget, 2.14× is not
  a headroom question but a regression investigation at soundd, escalated with
  the numbers that produced it.

**The reserve is measured on the same runs, and it has one branch.** R8 counts
what §1.2 charges to the reserve — interrupt service routines and scheduling
passes — per period, across gate A's four configurations, and reports it as a
permille of a CPU. At or below 100 permille the held-back fraction stands and
§1.3 loses the word *provisional*. Above it the reserve rises, and everything
derived from it falls with it: the admissible capacity, the real-time ceiling,
and the budget ceiling in the branches above. **That is escalated to the owner
as a design question with the measured numbers**, never absorbed by admitting
past a reserve the machine was measured to need — and never answered by deriving
a new constant, which is the thing this section's own history says does not
work. What the shipped machine has meanwhile is 100 permille of slack (§1.3) and
a counter that says when it is being spent.

---

## 10. What resists, and is not overridden

Four things in the existing law do not fit this design cleanly, and a fifth did
until §8 was rewritten. Each is recorded rather than quietly worked around, the
withdrawn one included — a resistance that dissolved is evidence about the
design that dissolved it.

- **The sleep locks owe a hold bound they do not currently declare.** §1.8's
  report composes a per-lock declared hold bound, and the rule that comes with
  it — a lock that declares none may not be taken where a real-time client can
  block on it — is an obligation on a set this document does not own. **The
  declarations land in `completion-architecture-spec.md` §9's lock table**, one
  bound per lock, beside the four rows that are already there; this document
  states the obligation and the checkable rule and writes no bound itself.
  `VFS` is the row that will resist: a holder inside device I/O can honestly
  declare nothing shorter than `USB_TIMEOUT_NS` = 2 s, so the rule's other half
  bites and the constraint is that no real-time path takes `VFS` — true of
  soundd today, and a thing R4 can check rather than assume.
  **`ProcessData` is the row this document cannot settle, and it is the owner's.**
  It is per process, every handle syscall takes the caller's own copy, and
  soundd's real-time thread takes it on every period; a holder inside an `fsync`
  parks under `{VFS, VOLUMES, XHCI}`, so an honest bound there is a device
  timeout too. Either that lock declares a bound its own paths meet, or the rule
  bites the shipped audio client and soundd's real-time cycle has to stop taking
  it. This document states neither a bound nor a constraint it knows the shipped
  machine breaks: R4 asks the static question, and the answer is a design
  decision rather than an arithmetic.
- **A device-driven client's period grid is anchored where its reservation was
  admitted, not where its stream begins.** §1.6 fixes the phase origin at
  admission, deliberately, so that no entity can re-phase its way into budget;
  the device's own grid is rebuilt whenever the pipeline drains
  (`audio-subsystem-spec.md` §2). The relative phase is therefore arbitrary, and
  §4.1 derives the wake-to-mix bound at phase zero and states what the other
  phases cost — a latency of up to one pipeline slot, absorbed, with throughput
  untouched. **Settled 2026-08-17: one anchor, and a measurement decides whether
  a second is ever owed.** A per-client grid anchor is a new mechanism, and
  inventing one to rescue a derivation is how each of the three superseded
  attempts began; what it would buy back is at most one slot of latency on a
  pipeline that carries eight, with no throughput and no dropout at stake. R8
  measures wake-to-submit on the four gate-A configurations, where an effect this
  size is visible if it is real — so this is answered by that measurement or it
  is not answered at all, and until then the phase origin stays where §1.6 puts
  it.

- **Real-time wake placement moves a thread across CPUs.**
  `scheduler-core-spec.md` §5's placement rule moves a woken real-time task to a
  sleeping peer when the waking CPU is itself running real-time work. Under
  §1.10 a reservation is admitted on one CPU, so that move either carries an
  admission check at a period boundary or does not happen. That sibling's §5
  therefore needs amending in the same chunk as R3, and this document does not
  pretend the two are independent. Worth recording beside it: under reservations
  the move's original motive is gone — two admitted real-time clients on one CPU
  are exactly what earliest-deadline dispatch with admission handles — so
  deleting the rule is available and would shrink R3. This document does not
  choose that, because the placement seam (§2) is where such a choice belongs.
- **The panicking duration kind was going to need a new constructor, and does
  not.** The previous form of §8 panicked on a wall-clock magnitude with a
  derivation attached, which `kernel/src/time.rs`'s closed set of kinds has no
  constructor for; this document required one and recorded the cost. §8's
  rewrites removed the requirement twice over — first by moving the panic to two
  assertions, then by deleting those too, leaving one fixed-hop `Tripwire` the
  file already has — so the change is withdrawn rather than carried. It is
  recorded here because the withdrawal is evidence about the principle: a design
  that needs the type system widened to express its panic is usually panicking
  on the wrong thing, and the second withdrawal says the same thing about a
  design that needs two assertions to say what one invariant already said.
- **The spec taxonomy says a document in this directory names no file, no test
  and no chunk.** This one does all three, as its two closest siblings already
  do — the completion architecture and the log architecture specs both carry
  chunk tables, file paths and gate names in this directory. The estate has a
  de-facto second class of document living here, and this document follows the
  siblings it must be read beside rather than the rule they already broke. That
  is a discrepancy for the owner to settle, not for this document to settle by
  choosing.
