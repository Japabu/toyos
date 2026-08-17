---
status: open
kind: finding
opened: 2026-08-16
---

# Placement is blind to caches and topology

Queued track from the 2026-08-16 owner conversation on scheduler direction.

A task's data is warm on the core it last ran on; migrating it throws that away.
`placement()` today balances load with no migration-cost model — the same
naivety that once produced the stopped-core preference defect. A production
placement story has three parts none of which we have:

- **Last-CPU bias with a migration-cost model**: prefer the previous core unless
  the imbalance pays for the cache refill.
- **Wake-affine vs spread**: on wake, decide between placing a task near its
  waker (shared data stays warm — pipe/IPC pairs) and away from it
  (parallelism). Today every wake ignores the waker.
- **Topology hierarchy**: SMT siblings share execution units (two busy siblings
  are not two cores), and cores cluster around shared caches (AMD CCX-style —
  two chatty tasks in different clusters pay for every exchange). Balancing
  should happen hierarchically across the machine's real shape, which the
  kernel currently does not model at all.

Composes with the reservation model (placement decides *where*, reservations
decide *how much*) and with the toyos-sched sim, which would need a topology
model to gate any of this. Independent of pipeline 2's remaining chunks.
