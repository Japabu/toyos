---
status: open
kind: track
opened: 2026-08-14
---

# CPU time is a band, not a reservation, so nothing anyone is promised is priced

Real-time work outranks normal work by right, fairness is a share of whatever is
left, and no entity is promised anything a number can check. Measured
consequence: **93.3 ms of audio starvation behind a fair storm**, in a 24-period
gap of 70 ms. Nothing below is built.

**The commitment: CPU time is a reservation.** Every entity holds a budget and a
period, admission refuses a set that does not fit, and no runnable entity is
served below what admission priced. One invariant replaces four pairwise
derivations that each held for one workload shape. **There is no band whose
precedence is unbounded in either direction** — both absolutes were tried and
both starve, and neither is expressible under reservations.

Exactly three entity kinds and no fourth: a real-time client, one fair class per
CPU, one dying server per CPU. A killed thread unwinding is the dying server's
work — not a right-holder and not a band. Interrupt and scheduling-pass time is
charged to a system reserve, to no entity.

## The chunks, and the invariant each must preserve

- **The reservation type, the per-CPU ledger, the admission arithmetic.**
  Admission is integer, checked, rounds against the holder, and runs per CPU
  against that CPU's own ledger, so overcommit cannot be admitted by a wrapped
  multiply.
- **The dying server as an entity**, with its queue, demotion, replenishment,
  timer discipline and preemption predicate. However many corpses a CPU holds,
  they are one entity with one reservation, so no count of them can close the
  rotation on anything. *Smaller than it was staged as: the corpse-aging cluster
  it was to dismantle has already left the tree.*
- **Real-time clients become reservation clients.** Dispatch is earliest
  deadline among entities holding budget, under a total tie-break, so a replay
  of the same choices dispatches the same entity and no band outranks another by
  right.
- **The urgency mark.** Precedence is bounded, totally ordered, and charged to
  the marked thread's own class, so no party's service depends on another
  party's behaviour. *Hard prerequisite: the simulator has no resource-holding
  op, so the waiter/holder trigger cannot be staged at all until one exists.*
- **The manifest row, the parser refusals, init's endowment check, the build
  gate.** A reservation is refused where it is created, by name, with the
  arithmetic in the message, and never observed later as a latency.
- **The service invariant and its scenario matrix.** The law and the instrument
  are one predicate, compared cumulatively rather than per period, and the
  fraction of periods actually compared is gated alongside the verdict.
- **The kill path's counters, its report, and its one fixed-hop tripwire.** The
  kill path asserts only over what its own CPU can observe and no workload can
  scale; everything composed is reported.
- **Measurement: the audio gate, soundd's budget, the system reserve.** A budget
  is twice the worst measured spend, and a measurement that will not fit is
  escalated to the owner, never absorbed by moving the ceiling, the floor or the
  reserve.

## Decisions already made, and the numbers behind them

- Capacity 1000 permille, system reserve 100 (**provisional and unmeasured** —
  everything resting on it moves when it is measured), admissible 900. Fair
  class 500 (5 ms per 10 ms), dying server 100 (1 ms per 10 ms), real-time
  ceiling 300. soundd is 580 µs per 2,902,494 ns = **200 permille**; 800
  admitted, 100 slack.
- **Exhaustion demotes into a work-conserving background tier** — never a
  throttle, never a silent extension. A reservation is a floor and never a cap.
- Replenishment happens only at a period boundary, on a grid anchored at
  admission that nothing moves. A waking entity may spend at most its
  utilisation times the time to its deadline. An overrun is charged back in full
  at the next boundary, which is why both coordinates are floored at the
  delivery granularity — 200,000 ns, which is 34.5 % of soundd's budget.
- **One urgency mechanism**: a mark, ordered ahead of unmarked threads inside
  its own class only, spending its own class's budget, totally ordered by set
  time, capped at half the class's budget, ending at 1 ms of running time. **No
  budget moves anywhere**, so a mark inside one process buys that process
  nothing. **Rejected, each falsified by an ordinary workload:** the lent band,
  the wake grant, the budget donation, and every derived bound on a blocked
  wait.
- The kill path asserts nothing of its own beyond one fixed-hop tripwire over
  the kick, its delivery and the drain. **Rejected:** an arm-time queue-depth
  snapshot (blind to in-flight siblings), a queue-occupancy assertion, and a
  progress marker — the last measured at **14.4 ms between two legal bumps** on
  a host port of the kernel's own allocator and ticket lock.
- Every sleep lock in the fixed set declares a hold bound, and a lock that
  declares none may not be taken where a real-time client can block on it. The
  filesystem lock can honestly declare nothing shorter than the 2 s device
  timeout, **so no real-time path may take it** — true of soundd today. **The
  process lock is the owner's open question**: soundd takes it every period, and
  a holder inside a sync parks under the whole device chain.
- Governance, and it is the point: four concepts and no fifth, three layers and
  two seams. **Every future feature enters as data or as policy, never as a new
  mechanism.** Heterogeneous capacity, topology, EEVDF, hierarchical shares and
  DVFS each cost one sentence and nothing until implemented.
- The manifest's period truncation is 0.11 ppm — one full period of slip per
  7.07 hours, against 21,638× headroom. 11 corpses is where the old rotation
  closed on itself.

**Two rules the current scheduler states as law that this design abolishes**, so
whoever lands it changes both deliberately: a ready real-time task always
preempting the normal band, and a real-time writer lending its band to a blocked
reader. Both are true of the tree today.
