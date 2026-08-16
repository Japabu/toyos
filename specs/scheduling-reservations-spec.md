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
executes is charged to one of them — against its budget while it holds one, and
against its background service afterwards (§1.5), but to an entity either way:

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

### 1.3 Admission

The **admission test** for one CPU is:

> the sum of the utilizations of the real-time reservations placed on that CPU,
> plus the dying server's utilization, plus the fair class's, may not exceed
> that CPU's capacity.

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

The two kernel-owned reservations, per CPU, as fractions of that CPU's capacity:

| quantity | reservation | utilization |
|---|---|---|
| capacity | — | 1000 permille |
| the fair class | 5 ms every 10 ms | 500 permille |
| the dying server | 1 ms every 10 ms | 100 permille |
| therefore the real-time ceiling | — | 400 permille |

**The 10 ms period is `QUANTUM_NS`** (`toyos-sched/src/fair.rs:16`), and the
choice is derived rather than inherited:

- *Bounded below by §5.3.* An entity whose period is shorter than a real-time
  client's puts more than one of its deadlines inside that client's period, and
  the client's latency term stops being one budget of each other entity. The
  fair class's period must therefore be at least the shortest real-time period
  on the CPU, which §4 fixes at 2 902 494 ns.
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
out is **slack** — capacity minus the admitted sum, per CPU — and slack belongs
to nobody. On the shipped machine (§4.1) 800 permille are admitted and 200 are
slack. Slack is not lost, because §1.5's background tier is work-conserving: a
CPU with only fair work still gives the fair band the whole machine. But no
guarantee rests on it, which is what makes it the honest place to pay for
everything this model cannot price exactly. The floor is a guarantee and not a
cap, and the same is true of every other reservation in the table: a reservation
is the least an entity gets, never the most it may have.

**Overcommit is refused where the reservation is created, by name**, and never
observed later as a latency:

- For the two kernel-owned reservations the check is static: the fair class's
  500 permille and the dying server's 100 are constants, their sum leaves the
  400-permille ceiling, and the constant that would break it does not compile.
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
  CPU that admission never priced.

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
per-period relative drift**. If a wake lands `s` before a boundary, the mix that
straddles it pre-spends at most `C − s` of the period it crosses into, and the
next wake lands `s + d` before the following boundary needing only `s + d` of
service before its own refill, so the per-period shortfall is `C + d − B`,
phase-independent, and it is negative for every `B ≥ C + d`. With `B = 580 000`
and the mix cost `C ≈ 203 175` (§4.1, and historical), the headroom is 376 825
ns against 87 ns of drift at 30 ppm — 4 328× — so the idealisation holds on the
shipped configuration and fails only for a budget sized to the mix cost, which
§4.1 forbids and §9.3's write-back transform goes on forbidding: a factor of two
leaves a headroom equal to the measured cost itself, which at today's figure is
203 175 ns against 87 ns of drift.

Both beats are computable and neither is hidden. From the manifest's truncation
alone the grid period 2 902 494 ns is *shorter* than the device's
2 902 494.331 ns, so wakes walk later against the grid and a grid period
occasionally receives no wake and never two: one full period of slip every
8 767 122 periods, 7.07 h. In the other direction a codec crystal fast by 50 ppm
puts two wakes inside one period every 20 047 periods, 58.2 s; the second of the
pair runs on `B − C` = 376 825 ns, which is 1.85× the mix it has to do.

### 1.7 The dying server

A killed thread unwinding its own stack is served by the **dying server**, an
ordinary reservation client that happens to be owned by the kernel rather than
by a process.

- Its runnable set is the CPU's queue of killed threads, served
  first-in-first-out. A killed thread is never migrated, so the queue it stands
  in is the queue of the CPU that owned it.
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

### 1.8 Two accounted mechanisms: the wake grant, and donation

The lent band is replaced by **two** mechanisms and not one. They are
structurally different — one flows from a waker to a wakee after a wait ends,
the other from a waiter to a holder while the wait lasts — and a single rule
that claimed to be both would be false of the shape it was not written for. Both
are accounted: no nanosecond either of them buys is spent by an entity that did
not pay for it, so §1.3's admission sum is unchanged by either.

#### 1.8.1 The wake grant

A real-time waker may attach a **one-shot grant** to a wake. Under the grant the
wakee runs against the **waker's deadline**, and every nanosecond it spends
there is charged to the **waker's budget**.

- **The grant creates no reservation.** It moves precedence and cost together;
  the alternative — precedence without cost — is exactly the unpriced promotion
  this document exists to abolish.
- **There is one pot and no double spend.** The waker's budget is drawn down by
  whoever is running under it, waker or wakee, so what the pair can spend in one
  period is one budget and not two, and the entity that runs out is demoted
  under §1.5 like any other.
- **The grant ends** at the first of: the waker's budget spent, the waker's
  period boundary, the wakee blocking, or the wakee being killed. It does not
  renew — a fresh wake is what carries a fresh grant, which is the shape of the
  thing it serves.
- **The grant places the wakee on the waker's CPU**, which is legal because a
  grant is only attached to a wake of a thread that holds no reservation of its
  own: a fair-band thread moves freely (§1.10) and adds nothing to the
  destination's ledger. A wakee that holds a reservation is pinned and is
  refused a grant — it does not need one, because it has a deadline of its own
  and admission already priced it.

This is what `soundd` signalling a client is (`audio-subsystem-spec.md` §4): the
client's fill window runs on soundd's deadline and out of soundd's budget, so
**soundd's reservation prices its clients' fills**, and §4.1's budget is a
budget for the whole cycle rather than for the mix alone. R8 measures the budget
*spend*, which is where those fills already appear — gate A runs a tone client
in every one of its four configurations.

#### 1.8.2 Blocked-donor donation

A thread blocked on a resource another thread holds **donates its deadline and
its budget to the holder** for as long as the block lasts. This is priority
inheritance, expressed in the only two quantities this model has.

- **It renews.** While the donor is blocked the donation is a standing fact, not
  a one-shot transfer: the holder draws the donor's budget as §1.6 replenishes
  it, period after period. "Ends when the donated budget is spent" is deleted
  as text — a donation that lapsed at the donor's first exhaustion would leave
  the holder finishing the critical section at background rates while the donor
  waits, which is inversion with the mechanism's own name on it.
- **Nothing is farmed, because the donor cannot spend what it is donating.** A
  blocked donor runs nowhere; the sum charged to its reservation in any period
  is still at most one budget.
- **It is transitive.** If the holder is itself blocked on a further holder, the
  donation follows the chain. The chain terminates because the kernel's sleep
  locks are a fixed, ordered set — `{ProcessData, VFS, VOLUMES, XHCI}`,
  `completion-architecture-spec.md` §7.4 — so no cycle exists and no chain is
  longer than four links.
- **It ends the instant the donor stops waiting**, and not at the holder's next
  park: a holder that parks *inside* the critical section still holds the
  resource the donor is waiting for, and dropping the donation there is the
  `old_park_kept_the_lend` break in reverse.
- **Nobody moves.** A donation never migrates a thread, which is what keeps it
  clear of §1.10's pinning and of invariant 7's never-migrate rule.

**Placement is why the two CPUs get different answers, and the difference is
priced rather than hidden.**

- *Donor and holder on one CPU.* The holder inherits in place: it runs against
  the donor's deadline and out of the donor's budget, **and** it goes on being
  the first thread its own entity dispatches, so the section is delivered at the
  sum of the two rates.
- *Donor and holder on different CPUs.* Budget does not cross a CPU, so the
  donation carries **ordering and not bandwidth**: the holder is dispatched
  first inside its own entity on its own CPU, and it is served at that entity's
  admitted rate and no more. Nothing migrates and no ledger changes.

In both cases the blocked client's wait is a stated, derived term rather than a
hope:

> `W_block` = summed over the links of the chain: the holder's remaining
> critical-section CPU time ÷ the utilization serving it — the holder's own
> entity, plus the donor's reservation when the two share a CPU — plus one
> period of the slowest entity in that sum, for the phase the block arrives in.

Every quantity in it is an admitted reservation: the fair class is 500 permille
of its 10 ms period wherever it runs, a real-time holder is whatever admission
gave it, and neither is a function of how many threads the workload put on that
CPU. A 2 ms critical section held by a fair-band thread is `2 ÷ 0.7 + 10` =
12.86 ms when the holder shares soundd's CPU and `2 ÷ 0.5 + 10` = 14 ms when it
does not, under a fair storm of any size. The same section without the fair
class's floor underneath it is `2C(k + 1)` and unbounded in `k`, which is the
inversion this mechanism exists to remove.

**"Dispatched first inside its own entity" is an input to §2's seam, and the
seam is amended to take it.** The reservation layer hands the intra-fair policy
one boolean — this thread is holding something a donor waits for — and the
policy must dispatch such a thread before threads carrying none. That is an
ordering input and not a reservation: the policy still cannot read a budget, a
deadline or a permille, and the marked thread accrues virtual runtime for every
nanosecond it runs, so its process pays for the jump and per-process fairness is
unmoved. The mark cannot be farmed either, because a thread cannot give itself
one: it takes a donor blocking on it, and the donor is the party that loses by
waiting.

The cross-CPU case is honest about what it does *not* buy: 14 ms is 4.8 device
periods, so a real-time client that takes a sleep lock inside its own period is
a client whose design is wrong, and this bound exists to make that a number
rather than a dropout. What it never becomes is unbounded.

### 1.9 The invariant

> **No runnable entity is served below its reservation.** For every entity, over
> every one of *its own periods* in which it was continuously runnable, the CPU
> time it received is at least its budget, measured on the wall clock.

This is the whole of the liveness claim, and it is stated in the same words
§6.1's I15 tests, deliberately: the law and the instrument are one predicate, so
there is no reading left for a harness to choose. The window is the entity's own
period grid (§1.6's phase origin), not a window that slides.

**It is delivered because admission and dispatch are the two halves of one
result.** §1.3 keeps the sum of the utilizations on a CPU at or below capacity,
§1.4's budgeted layer is earliest-deadline-first over reservations whose
deadlines are their periods, and §1.6's rejoin rule stops an idle entity from
carrying a claim into a later period. That is the classical implicit-deadline
EDF result: at a utilization sum of at most one, every entity meets every
deadline, which for a server means it receives its whole budget inside every
period it can use it in. The background tier cannot weaken it because it runs
only when no entity holds budget and is runnable.

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
late by at most `G` — one interrupt delivery plus the preempt-off section it may
land inside, bounded by `MAX_PASS_NS` = 200 000 ns
(`toyos-sched/src/cpu.rs:893`) — and an entity can overrun by at most `G`, which
§1.6 charges back at its next boundary. The guarantee above therefore carries
one `G` of tolerance, once, and never once per period: §6.1's I15 compares
*cumulative* service against *cumulative* owed budget for exactly that reason,
so a scheduler that spends `G` every period accumulates a deficit and reds
instead of hiding inside a per-period allowance. `G` is 34.5 % of soundd's
budget and 20 % of the dying server's, which is why it is a term this document
states rather than a rounding error it ignores.

Starvation is not bounded by any of this — starvation is unrepresentable under
it, because an entity that could be starved is an entity whose reservation was
admitted and therefore is not.

The wall clock is the measurement, deliberately: it is the clock §8's own
assertions are evaluated on, and a model that measures liveness on a clock the
kernel cannot read is a model that cannot see the failure the kernel dies of.

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
replenishment boundary, an exhaustion, a kill, and the arrival of a corpse. The
pass lands within `G` of the event — one interrupt delivery plus the preempt-off
section it may fall inside, bounded by `MAX_PASS_NS` = 200 000 ns
(`toyos-sched/src/cpu.rs:893`) — and for an event raised on another CPU the
message-and-kick hop is in front of it, which is the term invariant I4 already
prices. This is the rule §1.7 cites when it says the dying server is preempted
by §§1.4–1.6 with no rule of its own; before it was written, that citation
pointed at three sections that contained no preemption rule at all.

**What this costs and where it is paid.** `G` is the whole of the difference
between the model and the machine: it is why §1.6 charges an overrun back at the
next boundary, why §1.9 carries one tolerance rather than one per period, and
why the shipped machine's 200 permille of slack (§1.3) is a quantity worth
having. It is not a term any workload scales — one preempt-off section, once per
event, whatever `k` is.

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
  ordering input from the reservation layer**: a thread marked by §1.8.2 as
  holding something a donor is blocked on is dispatched before threads that
  carry no mark. That is a boolean and not a reservation, and the marked thread
  is charged for what it runs like any other. Anything else it does is
  therefore invisible to §1.9: no intra-fair policy can starve a real-time
  client or the dying server, and none can be starved by them, because the fair
  class's floor was admitted before the policy ran.
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
| `an_aged_grant_is_not_undone_by_a_pass_inside_its_chunk` (`cpu.rs:2521`) | **replaced by `an_interrupt_storm_does_not_take_the_dying_server_s_budget`.** The grant it protects does not exist, but its subject does: passes are handed out by every interrupt the machine takes, and the replacement asserts that a stream of them inside a dispatched entity's budget does not cost that entity its service. Under §1 the property holds by construction — budget is spent by running, not by being dispatched — which is exactly why it is worth a gate that would notice if an implementation reintroduced a claim a pass can reset. |

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
  §1.8.2's donor and holder are unrepresentable in a script that has no
  resource-holding op, so `old_park_kept_the_lend`'s re-derivation and the two
  donation gates cannot be staged until they exist. R4 owns that (§9.2).

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
  one quantum. It is amended to §1.8.1's wake grant — the same mechanism, priced
  — and the amendment is already recorded at the site so that the surviving law
  and this document do not disagree while R4 is unlanded.

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

`specs/audio-subsystem-spec.md`

- §4's "under the lent band" (`:110`), which names a mechanism §7.1 abolishes.
  It becomes soundd's wake grant (§1.8.1), and the cycle diagram means the same
  thing it always meant: the fill runs on soundd's deadline. Updated at the site
  in the same pass as the scheduler-core amendment above.

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
  bound carried a single `DYING_CHUNK_NS` = 1 ms. §5.3 re-derives it against the
  reservation instead, where the quantity that bounds real-time latency is the
  budget of the entities whose deadlines are earlier and not the number of
  corpses. The re-derived term is **larger** than the one it replaces —
  2 322 494 ns against 1 ms for soundd's CPU — and saying it is tighter would be
  this document flattering itself: the old number was smaller because it priced
  one corpse, and it was false for every `k` above one.

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
that is the only form the design delivers. soundd is woken at a boundary and is
continuously runnable from there until its mix is done, so §1.9 gives it its
whole 580 µs inside that period; therefore its first dispatch is no later than
`period − budget` = 2 322 494 ns after the wake, and its mix is complete by the
next boundary. Both numbers hold whatever else is admitted on the CPU, because
they come from the guarantee rather than from the interference sum — §5.3's
sharper term is the same bound arrived at from the other side, and the honest
statement is the smaller of the two. That is what "bounded by one device period
rather than by a quantum" means: 2 902 494 ns against `QUANTUM_NS` = 10 ms,
which is 3.445 device periods.

**The budget is an estimate, is declared as one, and prices the whole cycle.**
Under §1.8.1 a client's fill window runs on soundd's grant and out of soundd's
budget, so this number covers the mix *and* the fills it signals for. The only
recorded figure for soundd's CPU cost is roughly 7 % of a core — 203 175 ns of a
2 902 494 ns period — and `specs/reference/production-audio-baselines.md` §4.3
marks it **historical**: it described the pre-Stage-7a tree, where an idle
soundd mixed silence about 43 times a second, a behaviour deleted on 2026-07-31;
the standing recorded idle figure is a structural 0 %, and the reference bars
that 7 % from being quoted as a hardware number at all. So 580 µs is 2.855× the
cost of code that no longer runs, on a path that was never the mix path. It is a
placeholder with an order of magnitude and nothing more, and R8 is the chunk
that replaces it with a measurement — under §9.3's transform, not by writing
down what it reads.

The admission arithmetic for the shipped machine, on one CPU: 200 permille for
soundd, 100 for the dying server, 500 for the fair class — 800 of 1000, and the
200 that remain are slack (§1.3). The same set is admitted at every CPU count,
because the reservation is placed on one CPU and the rest of the machine is
unaffected by it.

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
> on that CPU reaches the head of one FIFO, the dying server delivers its
> declared reservation to the head, and the head advances or is inside a wait
> that has a bound of its own. The retirer is not given a deadline for the end
> of that chain; it is given two assertions on the CPU that delivers the
> service, each of which is false only if the kernel has a bug. What the retirer
> keeps for itself is the fixed hops at its own end — the kick, its delivery,
> and the drain that puts the victim in the queue.

§8 is the derivation, and the principle it turns on is that a bound is asserted
where the service is delivered and nowhere else.

### 5.3 Invariant I4 — real-time wake latency

I4's current bound is interrupt delivery, plus the longest preempt-off section,
plus the observation granularity, plus one dying chunk. The fourth term is
deleted (§3.2) and re-derived: what a real-time client waits for is not a
corpse's chunk but **the budget the other entities on its CPU may legitimately
spend before its own deadline arrives**.

> I4's bound is interrupt delivery, plus the longest preempt-off section, plus
> the observation granularity, plus `W`, where `W` is the **smaller** of two
> quantities: the sum of the budgets of the entities on that CPU whose current
> deadlines are earlier than the waking client's, and `period − budget` of the
> waking client's own reservation.

Both halves are derived and each is the tighter one in a different shape.

- *The interference sum* is what earliest-deadline dispatch can legitimately put
  in front of the client. Each other entity contributes at most one budget,
  because a period boundary moves an entity's deadline *later* and every
  entity's period is at least the client's (§1.3's lower bound on the fair
  period is exactly this requirement). Its closed form is unit-coherent: `Σ uⱼ ·
  Pⱼ` over the other entities, which is at most `(capacity − the client's own
  utilization) × the longest period on the CPU`.
- *The guarantee half* comes from §1.9: a client continuously runnable from its
  wake receives its whole budget inside the period it woke in, so it cannot
  still be waiting `period − budget` after a wake that landed at a boundary. The
  argument is a counterfactual over an interval on which the client is runnable
  throughout, and the schedule up to its first dispatch does not depend on what
  it does after it, so the counterfactual is exact.

On soundd's CPU the sum is 5 ms (the fair class) + 1 ms (the dying server) =
6 ms and the guarantee half is 2 322 494 ns, so `W` is 2 322 494 ns. The bound
is **larger** than the `DYING_CHUNK_NS` = 1 ms term it replaces, and it is a
bound in every workload rather than in the one-corpse case; the term it replaces
was smaller because it counted one corpse, and `k` corpses made it false.

**I4 also covers a client that blocks**, which the old form did not: a real-time
client blocked on a resource resumes within `W_block` (§1.8.2) of the block, and
`W_block` is derived from admitted reservations alone.
`old_donation_not_renewed` is the gate that reds here; `old_park_kept_the_lend`
reds on the other side of the same mechanism, where a donation outlives the wait
that justified it.

### 5.4 Invariant I14 — a retire reaches release

I14 keeps its clock — the wall clock, for the reason the current design already
records — and changes shape twice.

**The rate replaces the stretch factor.** An unwind's wall-clock cost is the
victim's own unwind time divided by the dying server's utilization, plus one
period for the phase the retire arrives in: with the server at 1 ms every 10 ms
the stretch is `period ÷ budget` = 10 exactly, so one quantum of unwind CPU is
100 ms of wall clock plus at most one 10 ms phase. The elevenfold factor is
deleted; it was the aged grant's delivery cadence — one chunk per
`DYING_AGE_NS + DYING_CHUNK_NS` — and the reservation does not have it.

**The `(1 + peers)` term goes away with the kernel deadline it modelled.** I14
becomes **head-relative**: it bounds the interval from the instant a corpse
reaches the head of its CPU's dying queue to its release, and the queue in front
of it is not inside the assertion at all. That is the same move §8 makes, for
the same reason — the composition of `n` waits is the sum of `n` bounds, and
writing the sum down as a constant is what forces a term the workload sets into
a place nothing can read it. The end-to-end retire-to-release time is then a
*consequence*: FIFO order (which `check_retires` already asserts) plus the
head-relative bound, summed over the corpses ahead. The sim can still print the
sum; nothing derives a constant from it.

> I14's bound, per corpse: from reaching the head of its CPU's dying queue,
> release completes within the unwind's own CPU time ÷ the dying server's
> utilization, plus one dying-server period of phase, plus the pass that frees
> the zombie.

I14 is not a special case of §1.9; it is a consequence of it, and it stays a
separate check because it measures the composition — the message hop, the rate,
and the zombie-freeing pass — rather than one entity's service.

### 5.5 The negative gates

The simulator's negative gates and its controls are law. Each is stated below as
surviving verbatim, re-derived, or replaced by a strictly stronger gate. **None
weakens, none loses the invariant that certified it, and none is dropped.**
Seven are added, and every added one names the invariant it must red under and
the cell of §6.2 it reds on — a must-red claim that does not name a cell is a
claim nobody can check.

| gate | disposition |
|---|---|
| `old_steal_port` | **survives verbatim.** The old steal-and-scan algorithm is a placement and ownership defect; reservations touch neither. |
| `old_commit_before_pass` | **survives verbatim.** The blocking protocol is untouched. |
| `old_preemptible_window` | **survives verbatim.** The registration window's preempt-off requirement is untouched. |
| `old_migrate_kept_the_corpse` | **survives verbatim.** A corpse handed to another CPU is a corpse taken away from the dying server that was admitted to serve it, which is if anything a sharper statement of the same break. |
| `old_rt_starved_the_corpse` | **replaced by a strictly stronger gate, and the arithmetic of "stronger" is below.** Its escape hatch stages "the real-time band outranks every corpse", which is one point of the design space this document deletes. Its replacement stages "the dying server is dispatched only when no real-time client is runnable" — the same break expressed against entities — on the same scenario shape, so it keeps the old gate's quantifier: **it must red under I14 on every seed**, which is the certification the old gate carried and the only one re-derived I14's rate term has. It **also** reds under I15, and on two shapes the old gate could not see. |
| `old_park_kept_the_lend` | **re-derived.** The break is unchanged — a park that keeps a lapsed window — but the window is now §1.8.2's donation, so the gate stages a park that keeps a donation after the donor stopped waiting, and the invariant that catches it is the donor's reservation being spent by a thread the donor is no longer blocked on. It needs `Op::Acquire`/`Op::Release` to be stageable at all (§3.2), which is R4's prerequisite and not a licence to land R4 without it. |
| `fair_share_per_thread` | **survives verbatim.** Per-process fairness is inside the fair class and unaffected. |
| `fair_double_charge` | **survives verbatim.** |
| `fair_identity_within_share` | **survives verbatim**, and gains a second job: it is the gate that holds §2's seam, since an intra-fair policy replacement that regressed thread identity would red here. |
| `overlong_pass` | **survives verbatim.** The pass budget is a property of a pass, not of a band. |
| `old_commit_fused` (control) | **survives verbatim**, and must still come back clean. |
| `fair_identity_tiebreak` (control) | **survives verbatim**, and must still come back clean. |
| **`old_unbounded_rt_precedence`** (new) | Stages the abolished rule: a real-time client is dispatched whenever it is runnable, ignoring budget and deadline. Must red under I15, on every seed, in the cell (real-time load = one client that attempts to exceed its reservation; corpses ≥ 1; band continuously occupied): the client spends past its budget, the dying server receives less than 1 ms in the period, and the deficit is the whole overrun. |
| **`old_aged_grant`** (new) | Stages *this document's predecessor* — per-corpse age stamps, a one-chunk grant ahead of the real-time band, and a restamp on every re-entry. Must red under I15 in two cells, and the arithmetic of each is below. The design being replaced becomes the gate that proves the replacement measures something. |
| **`old_arm_time_snapshot`** (new) | Stages the superseded §8: the retirer reads the victim CPU's queue depth when it arms and computes a wall-clock deadline from it. Must red on the concurrent-retire schedule — `m` independent retirers aiming victims at one CPU inside one drain window, every arm-time read returning zero while all `m` victims are already in flight — with the dying server honouring its reservation exactly throughout, so the red is "a deadline expired while every reservation was met". The review's probe `probe_arm_time_depth_is_blind_to_in_flight_siblings` is this gate's seed: it already demonstrates the blind read on four victims, and what the gate adds is the deadline the read was feeding and the release time it fails against. |
| **`old_underdelivered_dying_server`** (new) | Stages §8's first assertion failing: a scheduler that delivers the dying server less than its reservation while its queue is non-empty. Must red under I15 on every seed, in every cell with corpses ≥ 1 — this is the gate that proves A1 has teeth, and it is deliberately a direct break rather than a design, because A1 is the assertion a *kernel bug* trips. |
| **`old_stalled_head_corpse`** (new) | Stages §8's second assertion failing: a head corpse that is served and bumps no progress marker, with no declared wait to account for it. Must red under I14, in every cell with corpses ≥ 1. |
| **`old_donation_not_renewed`** (new) | Stages §1.8.2's live donation lapsing at the donor's period boundary, so the holder finishes the critical section at background rates. Must red under I4's blocked clause, in the cell (fair load = a storm; a real-time client blocked on a fair-band holder), where the wait grows with the storm and `W_block` does not. |
| **`old_unaccounted_wake_grant`** (new) | Stages §1.8.1's grant charging nobody — precedence without cost, which is the shipped lend. Must red under the accounting invariant: a nanosecond the wakee ran appears in no entity's budget, and the waker's spend for the period is below what it delivered. |
| **`many_victims_many_retirers_slow_device`** (new control) | `m` victims, `k` concurrent retirers, and an in-unwind device wait at its own timeout, on a CPU whose real-time client spends its whole budget every period. Must come back **clean**: this is the schedule every previous tripwire panicked on, and it breaks no rule. |

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
The fairness, ownership and accounting invariants are untouched. **Two are
amended and this document says so rather than listing them as untouched**: the
timer invariant gains §1.11's arming list (the running entity's exhaustion
instant and the boundaries that can change the winner join `quantum_end` and the
parked deadlines), and the boost-window invariant is re-derived at R4 into the
wake grant's terms — a window that ends at one of §1.8.1's four conditions, with
the time inside it charged to the waker's budget.

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
| the recurrence interval of those gaps | one gap per real-time period; one per dying-server period; one per three dying-server periods |
| fair load | idle; one thread; a storm |
| lock holding | none; a fair-band thread holds a lock a real-time client blocks on, same CPU and other CPU; a two-link chain |
| donation | live; lapsed at the donor's boundary; kept after the donor stopped waiting |

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
- **The lock and donation dimensions** exist because §1.8 is the one mechanism
  in this document whose failures are invisible to every other row: a blocked
  donor is not continuously runnable, so I15 opens no window on it, and the wait
  it suffers is charged to nobody. They are also the two dimensions the sim's
  `Op` vocabulary cannot yet express (§3.2).

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
- **`period_ns` outside `[200_000, 1_000_000_000]` is refused**, and the two
  ends are derived rather than round. Below `MAX_PASS_NS` = 200 000 ns
  (`toyos-sched/src/cpu.rs:893`) a period is shorter than the granularity the
  guarantee is delivered at (§1.9's `G`), so the reservation is one the machine
  cannot honour whatever the arithmetic says. Above one second the reservation
  stops meaning anything a real-time client wants — §5.3's latency term is *one
  period of each other entity*, and a legal 200-permille client with a
  3 600 s period would put a 720-second budget inside it — and the product
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
reservations and the two kernel-owned constants of §1.3 — so a machine
whose manifest overcommits fails at the first boot of that manifest, in the same
place and with the same shape as a manifest naming a right that does not exist.

### 7.3 Refusal wording

A refusal names five things: the program, the reservation it asked for, the CPU,
what is already admitted there, and what remains. It reads as arithmetic,
because arithmetic is what the reader has to check:

> `soundd: reservation 580000/2902494 ns is 200 permille; cpu0 has 400 permille
> for real-time work and 350 are already admitted (compositor 350); 50 remain.
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

## 8. The retirer's wait, and where the tripwire lives

**The panic condition is computed where the service is delivered, and it asserts
service delivery — never a wall clock against a remote snapshot.** That sentence
is the whole of this section; the rest is what it costs and what it buys.

### 8.1 What the previous form was, and why no repair of it exists

The form this replaces had the retirer read the depth of its victim's CPU's
unwind queue when it armed, and compute a wall-clock deadline from that depth
and the dying server's declared rate. It fails three ways, and each is fatal on
its own:

- **The snapshot undercounts, provably.** A retire is a message; a victim's
  position in the FIFO becomes real only when its CPU drains that message. So
  every arm-time read precedes every arrival, and `m` independent killers aiming
  victims at one CPU inside one drain window all read approximately zero while
  all `m` victims are already in flight. The review's probe
  (`probe_arm_time_depth_is_blind_to_in_flight_siblings`) demonstrates exactly
  this on four victims: four arm-time reads of zero, then one drain, then four
  corpses in FIFO order. The last victim is released after `m` unwinds and its
  retirer's deadline priced one.
- **The read is not the retirer's to make.** `scheduler-core-spec.md` invariant
  2 says a CPU's scheduler state is exclusively its own; the only published
  per-CPU numbers are a combined `rq + dying` load and a fair-only surplus, and
  publishing a dying depth would publish a value that is stale by construction —
  it can only be written at a pass, and the arm happens between passes.
- **No wall-clock deadline computed at arm time can price what arrives after
  it.** Not the corpses queued later, not a sleep-lock chain whose holder is on
  another CPU, and not a device wait inside the unwind — `close_all` flushes
  modified files, which reaches a USB transfer bounded by `USB_TIMEOUT_NS` = 2 s
  (`kernel/src/drivers/xhci/mod.rs:319`) per transfer, and a wait is not CPU
  time the reservation multiplies.

The common shape is that a *deadline* is a claim about a future the arming CPU
cannot see. Parameterising the constant moved the workload term from the queue
depth into the unwind length and left it exactly as reachable. So the deadline
goes, and what replaces it is not a bigger number.

### 8.2 The victim's CPU owns the tripwire, and asserts two things

Everything the wait depends on is locally checkable on the CPU that serves the
unwind, at the instant it serves it. Both assertions are false only if the
kernel has a bug, and neither has a term any workload sets.

**A1 — the dying server delivers its reservation.** Over each of its periods in
which its queue was non-empty, the dying server received at least its budget.
This is §1.9 restricted to one entity, checked by the CPU that owes it, which is
also the only CPU that can check it. Its violation means the scheduler did not
honour an admitted reservation: a kernel bug, and a panic.

**A2 — a served head corpse makes progress.** The corpse at the head of the FIFO
advances its unwind while it is being served. Progress is the unwind's own
monotone marker moving, and the marker is bumped in every loop whose trip count
userland sets — one bump per handle in the sweep, one per region in the
address-space teardown — so its cadence is a property of the kernel's code and
not of the process's size. A corpse parked on a lock or inside a declared device
wait is **not being served**: its progress clock pauses, and the wait it is in
carries its own bound (below). The tripwire fires when a full dying-server
budget of progress-clock time — 1 ms — has been delivered to the head with no
bump. The magnitude is derived and declared as an estimate: `MAX_PASS_NS` = 200
000 ns is the budget the kernel already asserts on a whole scheduler pass, which
is the longest straight-line stretch of kernel work it measures anywhere, so 1
ms of CPU inside one marker-to-marker stretch is five times a quantity the
machine treats as an upper bound already. That makes it absurd rather than
merely large — which is what makes it a panic and not a bound — and generosity
costs only a later panic on a bug that is real either way.

**The waits inside an unwind are bounded by somebody else's obligation, and the
chain terminates.** A corpse waiting on a lock waits for its holder, and every
holder is either alive — served by ordinary reservation law and, while a corpse
waits on it, running under §1.8.2's donation — or itself a corpse under A1 and
A2 on its own CPU. A corpse inside a device wait waits at most that device's own
declared timeout. The sleep-lock set is fixed and ordered
(`{ProcessData, VFS, VOLUMES, XHCI}`, `completion-architecture-spec.md` §7.4),
so no cycle exists and no chain is longer than four links; every link is finite;
the sum of finitely many finite waits is finite. That is the whole of the
liveness argument, and it contains no constant that a workload can grow.

### 8.3 What the retirer keeps

The retirer parks uncancellably for its victim's release, as it does today, and
it keeps **one fixed-hop tripwire** covering only its own end of the protocol:
the kill bit and the kick, the kick's delivery, and the drain that puts the
victim into the FIFO. Its terms are one interrupt delivery, one preempt-off
section (`MAX_PASS_NS` = 200 000 ns), and one pass prologue — no queue depth, no
unwind length, no device wait, and nothing at all that happens after the victim
is in the queue. Past that point the retirer has no deadline, because the
assertions above are what say the wait ends, and they are checked where they can
be.

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
panicked on a derived wall-clock magnitude. What survives here is a fixed-hop
`Tripwire` whose expiry is absurd (a kick that never arrives), an invariant
check that is not a duration at all (A1), and a progress budget whose magnitude
is absurd for the reason A2 states. `kernel/src/time.rs` is unchanged by this
design, and §10 no longer carries that item.

**One thing these assertions cannot see, stated rather than left implied.** A
CPU that has stopped taking interrupts entirely fails every deadline on it, A1's
period boundary included, and no assertion evaluated on that CPU can fire. That
is not a scheduling failure and its detector is not the retirer's wait.

### 8.4 What the sim prices, and what it does not

I14 (§5.4) is the head-relative bound and the sim reads its terms off the run,
which it can and the kernel cannot. With the dying server at 1 ms every 10 ms,
one quantum of unwind CPU costs `10 ms ÷ 0.1` = **100 ms of wall clock**, and a
victim behind `n` others is `(1 + n) × 100 ms` plus **one** 10 ms phase term —
the phase is paid once, when the retire arrives, because every later unwind
starts inside a period stream that is already running. The superseded
derivation's 110 ms per corpse was the aged grant's cadence, one chunk per
`DYING_AGE_NS + DYING_CHUNK_NS` = 11 ms; it is not this rate, and the claim that
the two agreed was arithmetic bent to make a rhetorical point. They differ by
10 %, in the direction that says the reservation delivers an unwind *faster*
than the design it replaces.

**The unwind's own length is an estimate and stays declared as one.** One
quantum of the victim's CPU time is a stand-in for handle closes and a teardown;
the reservation multiplies it, it does not measure it, and nothing in §8.2
depends on it — A1 and A2 are true of an unwind of any length, which is the
difference between this section and the two before it.

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
| **R1** | The reservation type, the per-CPU admission ledger, §1.3's policy constants — the fair class's own `(budget, period)` among them — and the checked admission arithmetic with its overflow refusal. Host code and tests only, no dispatch change and no behaviour change. | small |
| **R2** | The dying server: the entity, its reservation, its queue, demotion to the background tier and replenishment, **with §1.11's timer discipline and preemption predicate**, because an exhaustion nothing notices is not one. Deletes §3.1's aging cluster and dispositions its four crate gates in the same chunk, since a tree with both is a tree with two answers. Re-derives I14 head-relative, re-stages `old_rt_starved_the_corpse` — which is the only gate that certifies I14's rate term, so it lands with the term it certifies — and lands `old_aged_grant`. | large |
| **R3** | Real-time clients become reservation clients: earliest-deadline dispatch among entities with budget, the total tie-break, demotion at exhaustion, replenishment at the boundary with the overrun charged back, and the background tier's work-conserving order — the second half of §1.11 lands here with the entities that need it. Re-derives I4 and lands `old_unbounded_rt_precedence`. Amends `scheduler-core-spec.md` §5's real-time wake placement in the same chunk (§10). | large |
| **R4** | Both of §1.8's mechanisms: the wake grant charged to the waker's budget, and renewing transitive blocked-donor donation. **Prerequisite inside this chunk**: `Op::Acquire`/`Op::Release` in the sim's workload vocabulary, without which the donor/holder trigger cannot be staged and neither `old_park_kept_the_lend` nor `old_donation_not_renewed` exists as a gate. Re-derives the boost-window invariant and lands `old_unaccounted_wake_grant`. Amends `scheduler-core-spec.md` §3's pipe-lend paragraph and `audio-subsystem-spec.md` §4 to the mechanism the tree then has. | medium |
| **R5** | The manifest row, the parser's refusals (including the period bounds), init's endowment-time check, the refusal wording with its five things, and the build gate. | medium |
| **R6** | I15 in its cumulative form and the §6.2 scenario matrix with every dimension, including recurrence interval, lock holding and donation, with the liveness fraction reported. | medium |
| **R7** | §8 at the sites: A1 and A2 on the victim's CPU, the progress marker in the unwind's own loops, the retirer's fixed-hop tripwire in place of `GIVE_UP`'s end-to-end form, and `old_arm_time_snapshot`, `old_underdelivered_dying_server`, `old_stalled_head_corpse` plus the `many_victims_many_retirers_slow_device` control. Closes the queue-shaped-tripwire defect — by deleting the wait's end-to-end deadline rather than by parameterising it — and amends `scheduler-core-spec.md` invariant 7 and the completion spec's §7.2a and §7.3 to their reservation forms. | medium |
| **R8** | Gate A's thorough tier against the recorded sample, both audio configurations at both CPU counts, and soundd's budget re-measured and written back under §9.3's transform. | measurement |

R2 and R3 are the two large chunks and they are deliberately separate: the
dying server proves the mechanism against work nobody can observe from userland,
and only then does the audio client's guarantee depend on it. R7 grew from small
to medium when its subject moved from one deadline to two assertions, and that
is the right direction for a chunk that owns the only panic in this design. R8
is what decides whether the design ships, and it is a measurement rather than a
review.

### 9.3 R8's transform, and every branch it can take

**The rule, written once so that R8 has nothing to invent:**

> `budget_ns := 2 × the worst per-period budget spend measured`, rounded up to a
> round number.

Each half is derived:

- **What is measured is the budget *spend*, not soundd's thread time.** Under
  §1.8.1 a signalled client's fill runs on soundd's grant and is charged to
  soundd's budget, so the quantity that must fit inside `budget_ns` includes the
  fills. Gate A runs a tone client in all four of its configurations, so the
  measurement already sees one client's worth; the worst is taken across the
  four configurations *and* across a multi-client, resampling measurement run
  made for this purpose. That run is a measurement and not a fifth gate-A
  configuration: it neither joins `tests/audio-baseline.toml` nor gets a
  recorded sample, because adding a config to the gate is a change to the
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
The ceiling is 400 permille of the period = 1 160 997.6 ns, so the written-back
budget `B′` falls into exactly one of:

- **`B′ ≤ 580 000`.** The estimate was generous. The budget falls, the slack on
  that CPU grows, and R8 lands with gate A green.
- **`580 000 < B′ ≤ 1 160 997`.** The budget rises. Admission still passes on
  the shipped one-CPU set, and gate A's distributional comparison is what
  decides: the tone client's wake-lateness distribution either moved or it did
  not. **A red here is escalated to the owner as a design question**, with the
  measured numbers, and is never absorbed by re-recording the baseline or by
  trimming the budget until the gate goes quiet.
- **`B′ > 1 160 997`.** Admission refuses and the machine would boot without
  audio. **The ceiling does not bend and neither does the fair floor**: they are
  what makes every other guarantee in this document true, and a design that
  moves them to fit a measurement has stopped being an admission test. `B′`
  above the ceiling means a measured spend above 580 498 ns — 2.86× the
  historical 203 175 ns figure for a whole period of soundd's cost, and 2.86× is
  not a headroom question but a regression investigation at soundd, escalated
  with the numbers that produced it.

---

## 10. What resists, and is not overridden

Two things in the existing law do not fit this design cleanly, and a third did
until §8 was rewritten. Each is recorded rather than quietly worked around, the
withdrawn one included — a resistance that dissolved is evidence about the
design that dissolved it.

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
  constructor for; this document required one and recorded the cost. §8.2's
  rewrite removes the requirement — a fixed-hop tripwire, an invariant check and
  an absurd progress budget are all kinds the file already has — so the change
  is withdrawn rather than carried. It is recorded here because the withdrawal
  is evidence about the principle: a design that needs the type system widened
  to express its panic is usually panicking on the wrong thing.
- **The spec taxonomy says a document in this directory names no file, no test
  and no chunk.** This one does all three, as its two closest siblings already
  do — the completion architecture and the log architecture specs both carry
  chunk tables, file paths and gate names in this directory. The estate has a
  de-facto second class of document living here, and this document follows the
  siblings it must be read beside rather than the rule they already broke. That
  is a discrepancy for the owner to settle, not for this document to settle by
  choosing.
