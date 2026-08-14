---
status: open
kind: defect
opened: 2026-08-14
---

# A sysroot built *behind* main refuses every checkout on the machine, and the refusal's instruction cannot be followed

`toolchain::adopt_shared_sysroot` asks one question — does this checkout's
`toyos-abi`/`toyos`/`userland/libc` agree with the witness the sysroot was built
from — and then answers a *different* one: it assumes any disagreement means the
holder is **ahead** of main.

```
Standing::MatchesMain => panic!(
    "... the sysroot belongs to {}, for a change that is not on main yet.\n\
     **Wait for it to land and merge main.** This refusal then ends by itself.\n\
     Do not pass --claim-sysroot. ..."
```

A sysroot built from a checkout that is **behind** main produces the same
disagreement, and then:

- every linked worktree that matches main hits the `Standing::MatchesMain` arm,
  which `panic!`s *before* the `assert!(claim, …)` below it — so no flag reaches
  it and `--claim-sysroot` is not an exit even for someone willing to take it;
- the primary checkout hits its own guard at `toolchain.rs:621`, which refuses
  because `claimant(&rust_dir) != root`, with the same "wait for it to land"
  sentence;
- the instruction is unfollowable in both cases, because what the sysroot is
  missing is already on main. Nothing anybody merges can change the witness.

## Measured on this machine, 2026-08-14

`rust/build/toyos-sysroot-claimant` names
`/Users/jan/Dev/jan/toyos-logabi (branch wt/toyos-logabi)`, written
2026-08-11 06:50. That worktree is no longer in `git worktree list` and
`git ls-remote --heads origin` has no `wt/toyos-logabi`, so no landing is
pending and none can happen.

`differing_trees` names `toyos-abi/src`. Hashing that tree the way `witness`
does (`DefaultHasher` over each file's bytes) puts the disagreement in exactly
three files, and the recorded hashes are the ones the tree had **before**
`4e690d0`:

| file | recorded witness | this tree, and `origin/main` | at `4e690d0^` |
|---|---|---|---|
| `toyos-abi/src/hda.rs` | `8c16085e5f3843ee` | `6e17b40502a55e8e` | `8c16085e5f3843ee` |
| `toyos-abi/src/log.rs` | `6d83b4d930203cc1` | `bf343b9d38ebc95f` | `6d83b4d930203cc1` |
| `toyos-abi/src/virtio_sound.rs` | `afd2f6bbab1a141d` | `1120f2626f0cd859` | `afd2f6bbab1a141d` |

`git merge-base --is-ancestor 4e690d0 origin/main` succeeds: `4e690d0`
("specs: law, plans, assessments, reference — four places, one genre each") is
on main and has been since 2026-08-13. It is the commit that rewrote the
`specs/` paths those three files cite in their doc comments — a comment-only
change, which the witness hashes because it hashes bytes.

So the sysroot holds main-minus-one-commit, every checkout holds main, and the
build system tells all of them to wait for main.

## What it costs

`cargo run -- --build-only` and `cargo test` are unreachable from every
checkout on the machine until someone deliberately reclaims. Found while
building `userland/calc`, whose own host tests run and pass — the guest half of
that work could not be verified here at all.

## Notes for whoever fixes it

- The two existing entries are neighbours, not this:
  `specs/issues/build/claim-sysroot-livelocks.md` is two live claimants racing,
  and `specs/issues/build/primary-reclaims-the-sysroot-silently.md` is the
  primary taking a *live* claim. This is a claim whose claimant is gone.
- `standing()` asks whether *this checkout* differs from main. The question the
  refusal actually needs is whether the **sysroot** does, which is answerable
  without any cross-checkout git: hash `SYSROOT_SOURCES` at `origin/main` and
  compare that against the witness. A sysroot behind main is staleness, and
  `adopt_shared_sysroot` already has the right verdict for staleness one branch
  up — `std_fork_stale` rebuilds rather than refusing, "because staleness is not
  a claim".
- A claimant naming a worktree that no longer exists is a second, cheaper
  signal, and `claimant()` already returns the path.
