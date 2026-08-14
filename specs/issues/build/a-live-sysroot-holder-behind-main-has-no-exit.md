---
status: open
kind: defect
opened: 2026-08-14
---

# A sysroot behind main still refuses everybody when its holder is still on disk

The residual of the entry `stale-sysroot-refusal-has-no-exit`, deleted in the
commit that closed its instance — `git log --diff-filter=D -- specs/issues/`
has its measurements, and the slug is bare here because a pointer at a file that
was deleted resolves nowhere. What closed: `toolchain::holder_gone` reads a
claim naming a worktree that is not on disk as dead, and a dead claim makes the
disagreement staleness, so any checkout whose sources are main's rebuilds the
sysroot rather than waiting for a landing that already happened.

The same geometry with a holder that is **still there** is not closed, and the
ruling of 2026-08-14 scoped it out deliberately rather than loosen anything a
live holder is protected by — 2026-08-04 is what loosening that cost.

## The shape

A worktree claims the shared sysroot for an ABI change and builds it. The change
lands. Main then moves on over `toyos-abi/src`, `toyos/src` or
`userland/libc/src` again — on this machine `4e690d0`, a doc-comment-only commit,
because `toolchain::witness` hashes bytes. Now:

- every other checkout matches main, disagrees with the witness, and
  `toolchain::resolution` answers `Wait` — "the sysroot belongs to X, for a
  change that is not on main yet. **Wait for it to land and merge main.**" X has
  nothing left to land;
- X itself is in the same position the moment it merges main: it matches main
  too, so it has no standing to claim, and the record it is told to wait for is
  its own;
- `--claim-sysroot` from a linked worktree needs standing nobody has
  (`toolchain::tests::a_checkout_identical_to_main_has_no_standing_to_claim`).

The exit that exists is `cargo run -- --claim-sysroot` in the primary checkout,
which `toolchain::ensure` admits by the flag alone. It is deliberate, it is
undocumented in the refusal an agent is actually reading, and it is what every
wedged agent has to be told individually.

## What would close it

The first note in the deleted entry: the question the refusal needs is whether
the **sysroot** differs from main, not whether this checkout does. Hash
`SYSROOT_SOURCES` at `origin/main` and compare that against the witness — a
sysroot *behind* main is staleness, exactly as `toolchain::std_fork_stale` is,
and staleness is not a claim. A sysroot *ahead* of main is the real claim and
must keep refusing.

The gates that must not weaken while it is done:
`toolchain::tests::a_claim_whose_holder_is_there_is_still_a_claim`,
`a_dead_claim_does_not_hand_the_sysroot_to_a_checkout_that_differs`,
`a_checkout_identical_to_main_has_no_standing_to_claim` and
`a_checkout_behind_main_has_no_standing_to_claim`.
