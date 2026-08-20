---
status: open
kind: track
opened: 2026-08-20
---

# The eased merge law carries a threshold

An external review (2026-08-20, adopted by the owner) named the eased law for
what it is: a deliberate correctness/throughput trade, not an equivalent of
pre-merge composition testing. A branch green against an older `main` can
merge after other changes and create a composition that was never tested
before becoming authoritative. Platform constraints explain the choice; they
do not demonstrate it is cheap. So the trade is instrumented, and the
threshold and consequence are defined now, not after the first incident.

**The instrument measures, from CI history, on a fixed cadence:**

- post-merge red-`main` incidents (the push-triggered run on `main`'s tip);
- total and p95 time `main` spends red;
- merges landing before validation of the previous tip completed;
- failures caused by interaction between PRs that were independently green
  (the incident class strict testing would have caught).

**Threshold and consequence, defined now:** the expected rate is near zero.
One interaction-failure incident, or more than one red-`main` incident in a
rolling week, and the stronger serialization returns — batch landings under
the orchestrator, or the organization move that unlocks GitHub's merge queue
— as a mandatory response, not an aspiration. CLAUDE.md's landing bullet
points here.

**The three defense layers stay distinct**, per the same review, and none
substitutes for the others: (1) pre-merge composition testing (what the ease
traded away, what the threshold guards); (2) independent oracles (the
high-risk-change rule in CLAUDE.md); (3) long-horizon empirical testing —
boot storms, fault injection, sighting correlation, hardware observation
(the redlist's practice). Measure them separately; report them separately.
