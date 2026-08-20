---
status: open
kind: track
opened: 2026-08-20
---

# The swarm is not yet falsifiable

The external review of 2026-08-20, adopted by the owner: the swarm has strong
case studies and unusual discipline, and neither is evidence of scaling. The
standing research question is — **does adding another agent reduce
time-to-correct-integrated-software without increasing escaped defects,
coordination cost, integration delay, or human intervention?** Anecdotes and
output volume do not answer it. A standing metrics program does, reviewed on
a fixed cadence.

**Raw events first, summaries second.** For every defect, record:
- origin: pre-existing / introduced by current work / unknown;
- discoverer: implementing agent / independent agent / automated gate /
  human / runtime observation;
- escape boundary: branch / PR / `main` / release or real hardware.

Never collapse "bug found" and "bug caused" into one count: a good swarm
finds many old defects while introducing few; a bad one looks productive by
rapidly finding bugs it just created.

**The minimum metric set:** task-to-merge latency (median, p95); useful
merged changes per week; abandoned or rebuilt PRs; gate rejections; human or
orchestrator interventions per integrated change; agent-introduced defects
caught before merge vs escaping to `main`; pre-existing defects discovered;
integration wait time; cross-PR dependency depth; red-`main` exposure (the
threshold track's numbers); percentage of discoveries converted into durable
tests, invariants, or refusal rules; severity-weighted escaped-defect rate;
and, if feasible, useful integrated change per unit of agent compute.

**Work allocation is part of the same instrument** (review point fifteen):
merged work classified over rolling windows into kernel correctness/security,
verification and tooling, architectural debt reduction, hardware enablement,
user-facing features, and self-hosting/toolchain expansion — with the
standing priority that correctness and security outrank self-hosting, checked
by measurement rather than remembered.

**The reading, defined in advance:** more agents, more throughput, stable
defect and coordination rates — good scaling. Falling defect rates —
exceptional. Rapidly growing integration burden or escaped defects —
saturation. Little added throughput with rising coordination cost — negative
scaling. Whichever the numbers say, the swarm size follows the numbers.
