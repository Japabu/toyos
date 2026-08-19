---
status: open
kind: defect
opened: 2026-08-13
---

# Two files still gate on a scheduler-migration stage transition that finished

The scheduler-core rewrite is complete: no stage of it is outstanding, and root
`CLAUDE.md`'s register line for it no longer mentions one. Two other files still
describe a live stage transition as a standing constraint:

- `tests/CLAUDE.md:25` — "**A scheduler-migration stage transition gates on
  this tier.**" (gate A's thorough tier).
- A userspace-drivers stage row: "Same rule as a scheduler-migration stage
  transition, and for the same reason."

Each of these needs its own read to fix correctly: both claims likely just lose
the scheduler-migration clause, since gate A's thorough tier still gates
*something*.

**The third claim is gone with the file that carried it.** It was the stronger
case — "**The scheduler is at Stage 5 of 10 with Stage 6 (`Hw`) and Stage 7
(cutover) ahead**", driving a "stage 5 must land in a window between scheduler
stage transitions" sequencing rule for a staged console rewrite. That work was
deleted on 2026-08-16, its central recommendation lost — the shipped console is
per-CPU and there is no `CONSOLE_RING` — and it took the stale sequencing
constraint with it.
