---
status: open
kind: defect
opened: 2026-08-19
---

# The fatal report once failed to take the screen back from `/bin/console`

First sighting, CI only, one occurrence against one green isolated re-run in
the same job — a rate, not a classification. `src/redlist.rs` carries the row.

**What the harness printed** (PR #141 run 32306139422, `guest (3)`):

> `FAIL screen_console_panic: the fatal report never took the screen back from
> the console — which would make /bin/console a downgrade on the machine it is
> for` — at 96 s, against the suite's usual seconds, so the verdict looks like
> a handoff waited for and never observed rather than a wrong pixel.
> `ALONE: GREEN, and it was alone both times — nothing the harness controls
> differed, so it failed once and passed once.`

**Why it matters beyond the rate**: `--console-boot` exists for asking a
machine questions instead of reflashing it, and its promise is exactly that a
fatal report still wins the screen. If the panic path can lose that race under
load even rarely, the diagnostic machine lies precisely when it is needed —
the same shape as the pre-`hlt` family, where a rare loss is a real defect.

**What the sighting does not establish**: whether the report was never
composed, composed and never displayed, or displayed after the harness stopped
looking. The next red should capture which; the diff it rode on (workflow
triggers and CLAUDE.md prose, PR #141) writes no kernel byte and is not a
candidate cause.
