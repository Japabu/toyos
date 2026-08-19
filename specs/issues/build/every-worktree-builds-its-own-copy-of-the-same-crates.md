---
status: open
kind: defect
opened: 2026-08-19
---

# Every worktree builds its own copy of the same crates, and one shared target directory removes almost all of it

Measured 2026-08-19. Three worktrees on three different branches were pointed at
one `CARGO_TARGET_DIR` and built in turn:

| | crates compiled | wall clock | directory after |
|---|---:|---:|---:|
| `wt/toyos-tlbissue`, empty dir | 139 | 33.75 s | 1.4 GB |
| `wt/toyos-clippy`, same dir | **0** | **0.12 s** | 1.4 GB |
| `wt/toyos-p2impl`, same dir — a genuinely different branch | **3** | **0.40 s** | 1.4 GB |

**Cargo does not key artifacts on the worktree path.** Fingerprints are content
based, so even the workspace-local crates are reused when the source matches;
only what a branch genuinely changes is rebuilt. Three branches shared one
1.4 GB directory where today they hold about 36 GiB between them.

**The build-directory lock is not the obstacle it looks like.** Two concurrent
builds on the shared directory: the second printed `Blocking waiting for file
lock on build directory` once and finished in 1.88 s against the first's 1.45 s.
(The `package cache` blocking in the same output is the registry lock, which
already happens today with separate directories.)

**Incremental stays on.** Same tree, one-line edit to `src/redlist.rs`, rebuild:
**0.92 s with incremental, 3.67 s without** — both genuinely recompiling
`toyos-build`. It costs 413 MB of the 1.5 GB and buys 4× on the edit-rebuild
loop. The case for disabling it was arithmetic from the unshared world, where
that 413 MB was paid per worktree.

## How the directory is calculated

`<primary worktree>/target`, where the primary is what `toolchain::owner()`
already returns — the checkout that owns `rust/`, the rustup link and `main`.

That is a path join and nothing else: no `$HOME`, no XDG, no `%LOCALAPPDATA%`,
no temp directory, no platform crate. It works the same on macOS, Linux, Windows
and later on ToyOS, because joining a component to a path is the one filesystem
operation every system has. It is also not a new concept — worktrees already
share the primary's toolchain through exactly this function, which is why
`--worktree add` prints `toolchain … (shared, not copied)`.

**It degenerates to today's behaviour.** With no worktrees — CI, a fresh clone,
anyone's first checkout — `toolchain::owner()` returns the current root, so the
directory is `<root>/target` exactly as now. Nothing to special-case for CI.

It has to be visible to cargo however cargo is invoked, because agents type
`cargo test` by hand and not only through the build system. So `--worktree add`
computes it once at creation and writes an absolute `build.target-dir` into the
new worktree's gitignored `.cargo/config.toml`. A *relative* path would be
committable but is resolved against the config file, so it would encode an
assumption about where worktrees sit beside the primary — true today, silently
wrong the first time one is put elsewhere.

## What has to move with it

`hostws::target_dir` is the only function that answers "where did cargo put this
crate's output", with three call sites, all in `src/build.rs` (`:222`, `:231`,
`:615`). But **two sites bypass it** and build the path themselves:

- `src/build.rs:416` — `fs::create_dir_all(root.join("target"))`
- `src/pr.rs:330` — the same

Both would quietly recreate an empty `target/` in every worktree and make the
change look like it had worked while still leaking directories.

Everything else that mentions `"target"` — `sourcegate.rs:92`, `stamps.rs:38`,
`toolchain.rs:592` and `:785`, `forkcheck.rs:99-103`, `build.rs:1943` — is a
prune list for a directory walk. Those become no-ops when the directory leaves
the worktree, not wrong. No action, but they are not target-directory
computations and should not be edited as if they were.

## Unmeasured

Incremental state now lives in the shared directory too, so two worktrees
building the *same* crate from *different* source could alternately invalidate
each other's incremental cache. It would show as the edit-rebuild loop being
fast alone and slow when two agents work on one crate. Cargo may key incremental
per fingerprint and make this a non-issue; nobody has checked.

Whether cargo holds the build-directory lock through the *whole* of
`cargo test` — which here is a build plus roughly 120 s of QEMU guests — or
releases it once compilation ends. If it is held, a second agent waits two
minutes per verification cycle rather than two seconds. This is the one
measurement that could still change the shape of the answer.

Both are cheap and neither blocks writing the change; they decide whether it
needs a mitigation, not whether it is right.
