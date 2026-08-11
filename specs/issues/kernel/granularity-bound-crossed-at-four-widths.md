---
status: open
kind: defect
opened: 2026-08-01
---

# The scheduler crosses its own derived granularity bound at four of ten widths

Distinct from `per-process-fair-split-is-the-policy`, and deliberately not merged with it. That one says
fairness degrades as the machine widens. **This one says the shipped scheduler
exceeds a limit its own design implies** — a different and sharper statement.

The bound is derived from granularities the policy itself picked:
`lag_spread + (ΣT_i + 1) × (QUANTUM + max KernelSection + 2 × RUN_CHUNK)`. It is
crossed at **4, 6, 8 and 12 CPUs**, by 116, 324, 418 and 634 ms (bold in
`per-process-fair-split-is-the-policy`'s table).

**The gate handles this honestly rather than hiding it**, which is the part worth
preserving. It reds on `max(derived, recorded allowance)`, so a sampled scenario
is gated on not regressing — but `Outcome::fair_over_bound` records every crossing
of the *derived* bound regardless, and the sweep prints
`N ns PAST THE DERIVED BOUND on the recorded allowance`. **The allowance cannot
quietly become the standard**, which is the failure mode of every temporary
baseline and the reason most of them end up permanent.
