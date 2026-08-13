# The fork estate

Nothing is vendored: every forked crate is its own repository's `toyos` branch on a pinned upstream commit, consumed via `[patch.crates-io]`. `forks.toml` is the manifest — upstream, base commit, delta size, tier, PR status; keep it accurate.

- A fresh `git clone` + `cargo run` works with no setup: cargo fetches the forks.
- To edit a fork: clone it beside the monorepo and list it in `.cargo/config.toml` (gitignored — see `.cargo/config.toml.example`). Commit and push to the fork repo; the monorepo pins the branch. Fork clones are shared by every worktree: explicit paths, no `stash`, no branch switching.
- `git log <base>..toyos` in a fork is exactly the ToyOS delta — and exactly the future upstream PR.
- Forks depend on ToyOS crates by version, never by path: a path escaping the fork's repo cannot resolve once cargo checks it out alone.
- Every change is upstream-mergeable: `#[cfg(target_os = "toyos")]` as a new platform beside existing ones, never modifying cross-platform code; upstream's comment idiom and density govern; the ToyOS story goes in the commit message; a delta is reread as the upstream reviewer will read it.
- Publishing `toyos-abi`, `toyos` and `window` to crates.io is the one blocker for actual upstream PRs. Not needed for local builds.

## Std library rules (the rust/ submodule)

- Add ToyOS as a new platform alongside unix/windows/wasi — never hijack existing cfg gates. The rest are consequences:
- Prefer ToyOS-specific files: `sys/pal/toyos/`, `os/toyos/`, anything with `toyos` in the path.
- A cross-platform file is touched only to add a target arm at an existing platform-dispatch site — never to change cross-platform semantics or API shape. `library/alloc` and `library/core` have zero delta and keep it.
- Cherry-picking an already-merged upstream commit is early convergence and allowed. Copying an unmerged PR is not — it voids the promise that the fork delta is what an upstream PR would contain.

## The estate's blind spot

The fork estate is outside every check the tree runs on itself. "I enumerated the call sites" is only true if the enumeration covered `~/.cargo/git/checkouts/`.
