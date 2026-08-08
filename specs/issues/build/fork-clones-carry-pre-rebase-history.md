---
status: open
kind: finding
opened: 2026-08-07
---

# Every fork clone still carries its pre-rebase history, on no remote

`git rev-list <branch> --not --remotes` over the 13 clones finds **66 commits
reachable from no remote ref at all**, every one of them on a local `master` or
`main`: cpal 9, mio 11, socket2 8, libloading 8, stacker 6, getrandom 5,
target-lexicon 4, memmap2 4, ctrlc 3, tokio 3, russh 2, raw-window-handle 1,
softbuffer 1, winit 1. They are the original ToyOS work committed straight onto
each fork's `master` before the 2026-07-28 re-basing built the clean `toyos`
branches on pinned upstream bases — the commit titles are the tell (`Add brief
README for ToyOS fork orientation`, `Add .DS_Store to gitignore`, target-lexicon's
`hack: silence warnings`), and that cruft is exactly what `forks.toml`'s header
says was reverted.

**Nothing is lost, checked rather than assumed.** For every fork, `origin/toyos`
is identical to `master` on the ToyOS-specific paths or ahead of it: socket2,
mio, winit, softbuffer, stacker, libloading, ctrlc byte-identical; cpal's
`src/host/toyos/mod.rs` differs +108/-61 with `toyos` holding the newer futex
state machine and `PERIOD_FRAMES` that master's `AtomicBool`/`BUFFER_FRAMES`
predate; raw-window-handle's +44/-5 is the PR-alignment commit; memmap2's
`src/toyos.rs` exists only on `toyos`; getrandom's `toyos-0.2` carries it at the
0.2-era path `src/toyos.rs` rather than `src/backends/`.

So this is dead history, not work. But it is genuinely unpushed, which is the
honest answer to whether the estate is clean and pushed, and it is what makes
`git log --all` in any of those clones misleading. Deleting the local `master`
branches is the obvious close — outside the repo and the owner's call, and
explicitly not something an agent should do on its own, since a fork's history
is what an upstream PR is made of.
