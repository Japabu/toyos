---
status: open
kind: defect
opened: 2026-08-19
---

# `standing` asks the local `main` ref, so a worktree that is only current looks like a claimant

`src/toolchain.rs`:

```rust
fn standing(root: &Path) -> Standing {
    let mut ahead = vec!["diff", "--quiet", "main...HEAD", "--"];
```

`main`, spelled literally. The same module already has the answer to that
question and documents it: `main_ref` prefers `origin/main` — *"the local
remote-tracking ref: nothing here fetches, so this is what the last `--sync`,
`--pr` or `git fetch` left"* — and falls back to `main` only where there is no
remote. It has exactly one caller, `sysroot_content_landed`. `standing` does not
use it.

A linked worktree's `main` ref is the **primary checkout's**, and the primary
moves only on `cargo run -- --sync`. So `main...HEAD` is taken from whatever
commit the primary last synced to, and every commit `origin/main` has gained
since then is attributed to *this branch* as a delta of its own.

## What it costs

`Standing::Diverged` short-circuits `resolution` to `Resolution::Claim` before
`staleness` is consulted at all, so the worktree is told it *may* claim the
shared sysroot — and told it in the words reserved for a checkout holding an
unlanded ABI change:

```
This worktree does differ from main in those trees, so it is the one checkout
that cannot merge its way out: merge main first if that is enough, otherwise
pass --claim-sysroot to rebuild the sysroot from here
```

Merging main does not help — merging is what *caused* it. The instruction that
remains is the one `Resolution::Wait`'s own text calls destructive: a checkout
with no ABI of its own claiming the sysroot is the 2026-08-04 failure `standing`
was written to prevent, and here the module talks its caller into it.

Worse, it is reached exactly where the fix for the sibling defect lives.
`sysroot_content_landed` searches main's *history* for the sysroot's witness so
that a sysroot which is merely **behind** main is resolved as staleness and
rebuilt, taking nothing from anybody. That is precisely this machine's geometry
— and `Diverged` returns before it is called.

## Measured

Dev host, 2026-08-19, worktree `wt/toyos-dfault` (PR #125), merged up to
`origin/main` at `5c973e1`, primary checkout still on `ce58fdc`:

```
$ git diff --quiet main...HEAD -- toyos-abi/src toyos/src userland/libc/src
exit=1                                    # Diverged -> Claim
$ git diff --quiet origin/main...HEAD -- toyos-abi/src toyos/src userland/libc/src
exit=0                                    # MatchesMain -> staleness
$ git status --porcelain -- toyos-abi/src toyos/src userland/libc/src
                                          # nothing uncommitted
$ git diff --stat origin/main HEAD -- toyos-abi/ toyos/
                                          # no delta at all
```

The branch's whole delta is `tests/`, `issues/` and `src/redlist.rs`. The two
files `differing_trees` named, `toyos-abi/src/io_uring.rs` and
`toyos-abi/src/syscall.rs`, differ because `origin/main` gained clippy edits to
them after the primary last synced — somebody else's landed work, read as ours.

The sysroot's holder that day was `wt/toyos-fdleak`, and it has no unlanded
delta either:

```
$ git -C ../toyos-fdleak diff --stat origin/main...wt/toyos-fdleak -- \
      toyos-abi/src toyos/src userland/libc/src
$ git -C ../toyos-fdleak status --porcelain -- toyos-abi/src toyos/src userland/libc/src
```

Both empty. Nobody on this machine held an ABI change; the sysroot was simply
built before the clippy edits landed. Every worktree merged past the primary is
refused, and each one is invited to claim.

## The fix, and what it has to keep

Call `main_ref(root)` from `standing` and diff `<ref>...HEAD`. `main_ref`
returning `None` is already `Standing::Unknown`'s case — *"an unanswered
question is not permission"* — so the missing-ref arm is unchanged in meaning.

What must not be lost is why the range is `...` and not `main HEAD`: the doc
comment above `standing` records that a symmetric `git diff main` made a
worktree that had merely not merged somebody else's landed ABI change look like
one holding an unlanded change of its own. Switching the ref does not weaken
that; it fixes the second half of the same mistake, where the ref itself is what
is behind.

The fixtures at the bottom of the module construct repositories with a `main`
branch and no remote, which is why they never caught this. One that adds an
`origin/main` ahead of `main` is the gate.

## Encountered

PR #125 could not run `cargo test` at all: the harness panicked in
`src/toolchain.rs` before building the image, so the QEMU half of that PR's gate
went unrun rather than green or red. It did not claim.
