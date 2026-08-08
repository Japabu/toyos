---
status: open
kind: defect
opened: 2026-08-05
---

# `--claim-sysroot` livelocks against a second claimant, and the loser cannot build at all

Measured on 2026-08-05 with eight worktrees on the machine. A worktree holding
an edit to `toyos/src` must claim the shared sysroot to build; the claim writes
the witness *inside* the toolchain phase and then the build runs for another
four to six minutes. Anything that claims during that tail leaves the first
claimant refused again — and its next attempt pays a full `rebuild_std` to take
it back.

Observed directly: `--claim-sysroot` returned 0 at 00:39:01 and the very next
command, one second later, refused with "disagree about toyos-abi/src,
toyos/src". Four consecutive claim-then-test attempts lost the witness the same
way. Two earlier attempts in the same session won it, so the outcome is who
finishes last and nothing else.

The cost is not the rebuild, it is that **neither party can run a test between
losing and re-claiming**, so a gate that takes one minute cannot be reached
inside a six-minute cycle that another agent restarts. This is task #134's "no
arbitration" with a measurement on it. The fix wants the same shape as the rest
of the lock directory: the claim and the build that follows it are one hold, or
the witness is checked once and carried through the build rather than re-read
at each phase.
