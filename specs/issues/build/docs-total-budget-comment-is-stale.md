---
status: open
kind: finding
opened: 2026-08-09
---

# `TOTAL_BUDGET`'s comment records a measurement 1,943 bytes below the set it measures

`src/docs.rs:224` reads *"72,254 at the 2026-08-09 measurement"* beside
`TOTAL_BUDGET: usize = 80_000`. The set weighs **74,197** bytes:

```
$ wc -c CLAUDE.md kernel/CLAUDE.md userland/CLAUDE.md tests/CLAUDE.md src/CLAUDE.md
   37322 CLAUDE.md
   18613 kernel/CLAUDE.md
    6529 userland/CLAUDE.md
    5791 tests/CLAUDE.md
    5942 src/CLAUDE.md
   74197 total
```

Same on `origin/main` — `git cat-file -p origin/main:<each>` gives the identical
five numbers — so this is not a branch's doing.

The gate itself is fine: `the_claude_md_set_is_within_its_total_budget` reads the
files at run time and asserts against 80,000, so nothing is unchecked. What is
wrong is the number a reader uses to decide whether an addition fits. Believing
the comment, the set has 7,746 bytes spare; it has **5,803**. An agent budgeting
a paragraph across the root and two subtree files on the comment's figure would
write 6 KB and red the gate it was trying to respect.

The comment is a *why* for the constant carrying a measurement, which is the
shape CLAUDE.md's slop rule warns about: a figure in a comment has no gate on it
and drifts silently, where the same figure in an assertion message is printed by
the failure that needs it. The fix is either to delete the measurement and leave
the reasoning, or to have the assertion print `total` and `TOTAL_BUDGET - total`
so the number a reader wants comes from the run rather than from a comment.
Deleting it is the smaller change and loses nothing.
