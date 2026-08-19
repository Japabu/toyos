---
status: open
kind: defect
opened: 2026-08-05
---

# The primary checkout reclaims the shared sysroot silently

Found 2026-08-05 while giving `--claim-sysroot` its arbitration; not fixed.

A linked worktree whose `toyos-abi` differs from the sysroot's must pass
`--claim-sysroot`, which now announces itself and queues behind every run in
flight. **The primary checkout does the same thing on any ordinary build and
says nothing.** `toolchain::ensure` reaches `std_sources_stale` for
`Owner::Us`, which compares the witness against the
primary's own sources; a worktree's claim makes that comparison stale, so the
next `cargo run -- --build-only` in the primary rebuilds std from main's
sources, rewrites the witness, and the claiming worktree refuses from then on.
No flag, no announcement, and no sysroot lock — `build()` takes it only when
`--claim-sysroot` is passed.

It is the same defect the flag now has arbitration for, on the path used far
more often. It is not fixed here because the two shapes conflict: a suite run
holds the sysroot lock shared for its whole length, and the primary rebuilds std
from inside `build_test_image`, which the harness calls under that hold — so
taking the exclusive lock there would deadlock a suite run in the primary
against itself. The honest fix is that a stale sysroot *inside a run* is a
refusal rather than a rebuild, which changes the primary's daily path and wants
its own gate.
