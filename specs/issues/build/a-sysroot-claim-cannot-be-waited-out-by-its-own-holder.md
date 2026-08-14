---
status: open
kind: defect
opened: 2026-08-14
---

# A sysroot claim whose holder is the checkout reading it wedges every worktree on the host

`src/toolchain.rs`'s `resolution` has three ways out of a disagreement between a
worktree's `toyos-abi`/`toyos` and the shared sysroot: `Claim` (this checkout
has an ABI of its own), `Stale` (the holder is gone), and `Wait` (somebody else
holds it for something not yet on main). `#58` closed the case where the holder
had been deleted. **It does not close the case where the holder is the checkout
doing the reading.**

## What happened, 2026-08-14

`wt/toyos-logd` built at 14:48 with `toyos-abi` equal to main's, so the sysroot
recorded `1786711726 /Users/jan/Dev/jan/toyos-logd wt/toyos-logd`. `#57` then
landed `MAX_RECORD_MESSAGE = 992` on main, the worktree merged it, and its
sources became main's *new* ones — which the sysroot does not hold. From that
moment:

- **the worktree** got `Standing::MatchesMain` (byte-identical to main) with a
  claimant that exists, so `Resolution::Wait`, refusing with *"the sysroot
  belongs to /Users/jan/Dev/jan/toyos-logd (branch wt/toyos-logd) … Wait for it
  to land."* It was told to wait for itself, and `Wait` panics before
  `--claim-sysroot` is even consulted;
- **the primary checkout**, on main and carrying the same 992, was refused by
  `toolchain.rs:709` for the same record: *"rebuilding it here would take it
  from the one checkout that cannot merge its way out."* The checkout it was
  protecting had nothing left to land;
- **every other worktree** would hit the same refusal the moment it merged main.
  A worktree that had *not* merged still matched the sysroot and built fine, so
  the wedge arrives one agent at a time, on the merge each of them is told to do.

Both exits closed, on a promise nobody was left to keep — the same sentence
`holder_gone`'s doc comment already carries for the other case.

## Why the record was even there

`record_claimant` is written by every sysroot build, not only by a `--claim-sysroot`
one, so "the sysroot belongs to X" means "X built it last". A worktree with no
ABI of its own becomes a holder simply by building — and then becomes its own
jailer as soon as main moves under it.

## What unwedged it

`cargo run -- --build-only --claim-sysroot` **in the primary checkout, on main**.
That is the one branch of the code that both reaches `rebuild_shared_sysroot`
and is entitled to: the primary is on main, so what it puts in the shared
sysroot is what every other worktree will merge. The refusal that names
`--claim-sysroot` as "takes it back deliberately" is the only place any exit is
offered.

## The shape of a fix

A claim is a claim only while somebody could still land it. `resolution` already
asks whether the holder exists; it should also ask whether the holder has
anything to land — and a claimant whose recorded path *is this checkout* has
nothing to wait for by construction. Both cases resolve to `Stale`: rebuild from
main's sources and say why, taking nothing from anybody.

`toolchain.rs:709`'s primary-side assertion needs the same question. It already
filters `who != root`, so it is only the `holder_gone` half that is too narrow
there.
