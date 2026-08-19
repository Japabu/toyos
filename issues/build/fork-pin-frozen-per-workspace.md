---
status: open
kind: defect
opened: 2026-08-07
---

# A fork pin is a moving branch frozen per workspace, and nothing re-reads it

`[patch]` names a *branch*; a lockfile records the rev that branch pointed at
when that workspace was last resolved. Five workspaces in the tree resolve fork
branches independently — the root, `userland/`, `tests/toyos-rust-tests/`,
`tests/toyos-rust-tests/tls-cranelift/`, and the `rust/` submodule (which has
two of its own). Push to a fork and every one of them keeps the old rev until
somebody happens to run `cargo update` in it. There is no mechanism that
notices, and no build that fails.

> Six when this was filed. `toyos-cc/` was the sixth and is now a member of the
> host workspace rather than a root of its own, so it resolves in the root
> `Cargo.lock` and `target-lexicon` is pinned in two lockfiles instead of
> three. The defect is unchanged in kind — the remaining five still freeze
> independently, and nothing re-reads any of them.

Measured 2026-08-07, before the fix in this branch: six pins were behind their
branch head, and two crates were pinned at *two different revs at once*.

- `libloading` — `fa0abe77` in `rust/Cargo.lock`, `2ca5f54b` in both
  `userland/Cargo.lock` and `tests/toyos-rust-tests/Cargo.lock`.
- `target-lexicon` — `45832ce6` in
  `rust/compiler/rustc_codegen_cranelift/Cargo.lock`, `50da81b3` in `Cargo.lock`,
  `toyos-cc/Cargo.lock` and `tests/toyos-rust-tests/tls-cranelift/Cargo.lock`.
  Two commits, and the one in the gap is semantic: `9aeabf5` adds
  `OperatingSystem::Toyos` to the SysV arm, so `Triple::default_calling_convention()`
  answered `Err(())` for ToyOS in `toyos-cc` and `SystemV` in cranelift. It was
  latent only because `CallConv::triple_default` folds `Err(())` into `SystemV`
  anyway (`cranelift-codegen-0.128.4/src/isa/call_conv.rs`) — a consumer that
  treated the error as an error would have diverged outright.
- `mio`, `socket2`, `tokio` — one `.gitignore` commit behind in
  `userland/Cargo.lock`; hygiene only.
- `raw-window-handle` — `76c4971c` where the branch head was `c39042b5`. This
  one had teeth: `forks.toml` claims the `toyos` branch is byte-identical to the
  head of PR #223, and that claim is about the *branch*, so the pinned tree was
  the pre-alignment one. The suite was validating code we had not sent and not
  validating code we had.

The `rust/` submodule's own eight pins were all at branch head, so this is a
monorepo-side drift, not an estate-wide one.

**The check that would catch it does not exist, and its shape is a decision for
the owner.** Compare each lockfile's `git+…#rev` against `git ls-remote <url>
<branch>` and fail on a mismatch. That catches all six. It also puts GitHub on
the path of whatever runs it, so it must not be a `cargo test` member or part of
the landing gate — an on-demand `cargo run -- --check-forks`, run when the estate
is touched, is the shape that costs nothing when the network is down. The purely
offline alternative — assert every lockfile agrees with every other about a
`(repo, branch)` pair — needs no network but is vacuous in a worktree, where
`rust/` is not checked out: it would have caught neither `libloading` nor
`target-lexicon` from where an agent actually works, and nothing at all for the
four crates that appear in exactly one lockfile. Not worth building.
