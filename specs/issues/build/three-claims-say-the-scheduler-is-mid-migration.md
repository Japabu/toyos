---
status: open
kind: defect
opened: 2026-08-13
---

# Two files still gate on a scheduler-migration stage transition that finished

The scheduler-core rewrite is complete — `specs/scheduler-core-spec.md`'s own
current-state section names no outstanding stage, and root `CLAUDE.md`'s
register line for it no longer mentions one. Two other files still describe
a live stage transition as a standing constraint:

- `tests/CLAUDE.md:25` — "**A scheduler-migration stage transition gates on
  this tier.**" (gate A's thorough tier).
- `specs/plans/userspace-drivers-spec.md:447` — stage 7's row: "Same rule as a
  scheduler-migration stage transition, and for the same reason."

Each of these needs its own read to fix correctly: both claims likely just lose
the scheduler-migration clause, since gate A's thorough tier still gates
*something*, per `specs/testing-strategy.md`.

**The third claim is gone with the file that carried it.**
`specs/plans/console-records.md:230` was the stronger case — "**The scheduler is
at Stage 5 of 10 with Stage 6 (`Hw`) and Stage 7 (cutover) ahead**", driving a
"stage 5 must land in a window between scheduler stage transitions" sequencing
rule for that plan's own staged work. That plan was deleted on 2026-08-16: its
central recommendation lost (the shipped console is per-CPU and there is no
`CONSOLE_RING`), so a plan that dies on completion took its stale sequencing
constraint with it, and the plan-content question this entry asked about it no
longer has a plan to be asked of.
