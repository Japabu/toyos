---
status: open
kind: finding
opened: 2026-08-01
---

# A fork depended on `toyos-abi` by **git** — the third case the rule does not cover

CLAUDE.md's rule is that forks depend on ToyOS crates *by version, never by path*,
with the reason given: a path escaping the fork's own repo cannot resolve once
cargo checks it out alone. **A git dependency is the third case, and it is worse
than the path case — because it resolves.** Silently, against a frozen snapshot,
with nothing to announce it. `toyos-abi` is the crate where a split-brain does the
most damage.

It happened here. `~/.cargo/git/checkouts/toyos-abi-9a70838a07f829d2/2fe0c57`
holds a `toyos-abi` that is not a slightly older monorepo copy but a substantially
different ABI: **seven files the monorepo does not have** (`gpu.rs`, `message.rs`,
`poll.rs`, `raw_net.rs`, `services.rs`, `shm.rs`, `system.rs`) and **missing two it
does** (`boot.rs`, `io_uring.rs`).

**Not currently live — established, not assumed.** Enumerating all fifteen
checkouts found the git form in exactly two *stale* getrandom commits
(`bb423bc`, `c473bb1`, both `toyos-abi = { git = ... }`). The three getrandom
commits actually pinned (`4659241`, `d304544`, `e05f79d`) all use
`toyos-abi = "0.1"`, and **no lockfile in the tree references
`Japabu/toyos-abi` as a git source at all.** What remains is inert cargo cache.

Filed anyway, because the near-miss is the finding: the violation occurred, ran,
and was corrected without anything in the tree ever reporting either event. The
rule should name the git case explicitly rather than leaving it to be inferred
from the path case's reasoning. Sweep the estate for the pattern when it next gets
path overrides.
