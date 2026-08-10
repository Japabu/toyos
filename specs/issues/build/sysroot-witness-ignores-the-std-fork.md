---
status: open
kind: defect
opened: 2026-08-10
---

# A worktree's `rust/library` edit does not rebuild the sysroot

`adopt_shared_sysroot` decides whether to rebuild std by comparing a *witness*
against the shared tree's recorded one (`src/toolchain.rs`, `witness(root)` /
`witness_path(rust_dir)`). The witness is a hash of `SYSROOT_SOURCES` —
`toyos-abi/src`, `toyos/src` and `userland/libc/src` — and of nothing else.

`rust/library` is not in it. So a checkout that edits the std fork and nothing
else builds against the std that is already there, silently: the sources on
disk say one thing and the sysroot says another, and the only symptom is the
compiler refusing a method the fork's working tree plainly has.

Measured on `wt/toyos-endow`, 2026-08-10, three times in one chunk. The tell is
`no method named <the one you just added> found for struct Child`, on a build
whose own log shows no `Building stage1 library artifacts` line at all.

## Why the witness is right about what it covers

It exists for a different question — "was this sysroot built from *my* sources
or another checkout's?" — and for the three trees it names it answers correctly.
`assert_std_built_from` is stronger still: it reads cargo's dep-info after a
build and refuses a sysroot naming a `toyos-abi/src` or `toyos/src` file outside
the building worktree. Neither is a staleness check on the fork itself, and
nothing else is either.

## Working around it

Delete the witness before the build:

```
rm -f /Users/jan/Dev/jan/toyos/rust/build/toyos-sysroot-witness
cargo run -- --build-only --claim-sysroot
```

The claim path then rebuilds unconditionally. This is what the endowment branch
did for every std edit in chunks 7 and 8.

## What it should be

The witness should cover `rust/library` for the same reason it covers the three
SDK trees: it is a source tree the sysroot is built from and a worktree may
legitimately differ from the shared checkout in it. Hashing the whole of
`rust/library` on every build is not free — it is thousands of files — so the
cheap form is the `*toyos*`-pathed files, which is the fork's own delta and the
only part a worktree edits. `find rust/library -path '*toyos*'` is 23 files.

A hash over those is not a complete answer — a cherry-picked upstream commit
outside them would still be missed — and it covers every case this tree has
produced.
