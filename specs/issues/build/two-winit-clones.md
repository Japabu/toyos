---
status: open
kind: defect
opened: 2026-08-07
---

# Two winit clones exist, and the canonical one is the stale one

`/Users/jan/Dev/jan/forks/winit` is at `be9ec72c`; `/Users/jan/Dev/jan/winit` is
at `faf99eb7`, which is `origin/toyos` and the rev every lockfile pins. Both are
clean and on `toyos`. `.cargo/config.toml.example` documents `../forks/<name>` as
the path-override convention, so `forks/winit` is the one an agent told to edit
"the winit fork" will find and path-override — and it is a commit behind, which a
path override silently substitutes for the pinned tree.

Outside the repo, so no commit fixes it: the owner should delete
`/Users/jan/Dev/jan/winit` (nothing is unpushed in it) or fast-forward
`forks/winit` and delete the stray. No other fork has a duplicate — checked
across all 13 clones under `forks/`.
