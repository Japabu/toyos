---
status: open
kind: defect
opened: 2026-08-13
---

# Three files still gate on a scheduler-migration stage transition that finished

The scheduler-core rewrite is complete — `specs/scheduler-core-spec.md`'s own
current-state section names no outstanding stage, and root `CLAUDE.md`'s
register line for it no longer mentions one. Three other files still describe
a live stage transition as a standing constraint:

- `tests/CLAUDE.md:25` — "**A scheduler-migration stage transition gates on
  this tier.**" (gate A's thorough tier).
- `specs/plans/userspace-drivers-spec.md:447` — stage 7's row: "Same rule as a
  scheduler-migration stage transition, and for the same reason."
- `specs/plans/console-records.md:230` — the stronger case: "**The scheduler
  is at Stage 5 of 10 with Stage 6 (`Hw`) and Stage 7 (cutover) ahead**,"
  driving a "stage 5 must land in a window between scheduler stage
  transitions" sequencing rule for that plan's own staged work.

Each of these needs its own read to fix correctly: `tests/CLAUDE.md`'s and
`userspace-drivers-spec.md`'s claims likely just lose the scheduler-migration
clause (gate A's thorough tier still gates *something*, per
`specs/testing-strategy.md`); `console-records.md`'s "stage 5 must land in a
window between scheduler stage transitions" sequencing constraint needs
someone to decide what it means now that there is no window to land inside —
that one is a plan-content question, not a wording fix.
