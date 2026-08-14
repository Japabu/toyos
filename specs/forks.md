# Forks

Every forked crate is a `toyos` branch in its own repository, based on a pinned upstream commit and consumed via a `[patch.crates-io]` git dependency. `forks.toml` records each fork's upstream, base commit, delta size, tier and PR status, and must stay accurate.

- A fresh `git clone` + `cargo run` needs no setup: cargo fetches the forks.
- To edit a fork, clone it beside the monorepo and list it in `.cargo/config.toml` (gitignored; see `.cargo/config.toml.example`). Commit and push in the fork repository; the monorepo pins the branch. Fork clones are shared by every worktree: explicit paths, no `stash`, no branch switching.
- `git log <base>..toyos` in a fork is the ToyOS delta and the content of a future upstream PR.
- Forks depend on ToyOS crates by version, never by path: a path outside the fork's repository cannot resolve once cargo checks the fork out alone.
- Every change must be upstream-mergeable: ToyOS enters as a new platform under `#[cfg(target_os = "toyos")]` beside the existing ones, cross-platform code is not modified, and comments follow upstream's idiom and density. The ToyOS rationale goes in the commit message.
- Publishing `toyos-abi`, `toyos` and `window` to crates.io is required before upstream PRs can be opened; local builds do not need it.

## Std library rules (the rust/ submodule)

- Add ToyOS as a new platform alongside unix/windows/wasi; never repurpose an existing cfg gate.
- Prefer ToyOS-specific files: `sys/pal/toyos/`, `os/toyos/`, anything with `toyos` in the path.
- A cross-platform file is touched only to add a target arm at an existing platform-dispatch site, never to change cross-platform semantics or API shape. `library/alloc` and `library/core` have zero delta.
- Cherry-picking an already-merged upstream commit is allowed. Copying an unmerged PR is not: the fork delta must remain exactly the content of a future upstream PR.

## Coverage

The fork sources live outside this repository, so repository-wide checks and searches do not reach them. An enumeration of call sites must also cover `~/.cargo/git/checkouts/` or the local fork clones.
