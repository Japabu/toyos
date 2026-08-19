---
status: open
kind: defect
opened: 2026-08-19
---

# Nothing ever reclaims a worktree, and on 2026-08-19 the dev host filled up

`cargo run -- --worktree add` refused on 2026-08-19 with its own disk check:

```
/Users/jan/Dev/jan has 24.7 GiB free and a worktree's target directories reach about 12 GiB.
```

The check (`src/worktree.rs:75`) was right, and it is the only part of this
subject the tree has. **There is an `add`, there is a `remove`, and nothing ever
calls `remove`.** Thirty worktrees had accumulated; sixteen of them sat on
branches already merged into `origin/main`.

Measured that day, before the cleanup:

| | |
|---|---|
| worktrees | 30 |
| `target/` directories under them | **153** |
| their total size | **129 GiB** |
| free space on the volume | 20 GiB of 926 GiB (98 % used) |
| worktrees whose branch was already in `origin/main` | 16 of 30 |
| commits existing only in a worktree | **0** — every branch was pushed |

## Why one worktree is ten target directories, not one

`Cargo.toml`'s `exclude` list keeps `bootloader`, `kernel`, `userland`, `toyos`
and `tests` out of the host workspace, and the reason is stated there and is
correct: each is cross-compiled by a `.cargo/config.toml` below it to a target
no host test can run, and cargo will not merge an inherited `build.target` away.
An excluded crate resolves on its own, so it builds into its own `target/`.

That is five. The rest are the guest fixtures under
`tests/toyos-rust-tests/` — `tls-cranelift`, `tls-dlopen-lib`, `tls-lib`,
`tls-multi-crate` — plus `userland/libc`, each its own project by the same
argument. Ten directories per worktree, every one legitimate, and `cargo clean`
at the root reaches exactly one of them.

The largest single worktree was 12 GiB. Within one of them, `target/debug`
alone held 1.9 GiB of `deps` and 1.7 GiB of `incremental` — a rebuild cache for
a branch that had already landed.

## The second finding: a workspace member with a private target directory

Eleven crates in the **primary checkout** held their own `target/` while being
listed in `Cargo.toml`'s `members`. A member builds into the workspace root's
`target/`, so each of these is an orphan that nothing has written since the
crate joined the workspace, and nothing will ever write again:

| crate | size | last written |
|---|---|---|
| `toyos-cc` | 4.0 GiB | 2026-08-14 |
| `toyos-ld` | 877 MiB | 2026-08-14 |
| `toyos-sched` | 1.1 GiB | 2026-07-31 |
| `kernel-loom` | 122 MiB | 2026-08-12 |
| `toyos-fat32` | 108 MiB | 2026-08-01 |
| `bcachefs` | 92 MiB | 2026-08-01 |
| `toyos-abi`, `toyos-gpt`, `toyos-ps2`, `toyos-keymap`, `toyos-desktop` | 122 MiB | 2026-07-31 – 2026-08-05 |

`tests/target` was a twelfth, 2.5 GiB, last written **2026-03-17** — five months
stale, from a build layout that no longer exists.

`src/hostws.rs` already holds the `exclude` list against the tree, so a crate
that joins neither `members` nor `exclude` is a red. What nothing notices is a
crate that joined `members` and left its old private cache on disk. Deleting all
twelve and rebuilding the workspace took 28.5 s and recompiled one crate, which
is the proof they were dead.

## What was done

`target/` deleted under every worktree; the sixteen merged worktrees removed
with `--worktree remove` (their branches are untouched and every commit is on
`origin`); the twelve orphans deleted from the primary. 20 GiB free became
157 GiB free. `cargo build --workspace` green afterwards.

The one worktree carrying uncommitted work that was *not* superseded —
`wt/toyos-nospecs`, 85 modified files of the `specs/` teardown — was left
standing. `wt/toyos-logspec` had one modified file whose subject had since been
deleted on `main`; that diff was saved to `toyos-salvage/` before its worktree
went.

## What is not done — the recurrence

Nothing above prevents any of it happening again, and it will: the accumulation
took about three weeks.

`--worktree list` (`src/worktree.rs:111`) prints the toolchain owner and then
defers to `git worktree list`. It does not say how large a worktree is, or
whether its branch is already in `origin/main` — the two facts that decide
whether it should still exist. A `list` that showed both would have made this
visible at worktree five instead of thirty.

The stronger version is reclamation rather than reporting: a worktree whose
branch has landed has no reason to hold 12 GiB, and `--sync` already runs at
exactly the moment that becomes true. Whether it should offer, do it, or only
name them is a design question this file does not decide.

**Not proposed: a gate.** Disk exhaustion announces itself — `--worktree add`
already refused, by name, with the number. The defect is that the refusal was
the *first* notice.
