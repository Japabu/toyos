---
status: open
kind: finding
opened: 2026-08-13
---

# A kernel doc comment cites a `known-issues.md` that stopped existing before this branch

`kernel/src/drivers/xhci/mod.rs:359` (added `17a6b17`, 2026-08-08): "*... and
that is the whole of what the T14's audio pops are made of*", followed by a
citation of §4 of a `known-issues.md`. That file is the monolithic one
`issues/` replaced (`issues/README.md`'s `opened` field still
explains the split); it does not exist on any branch found by
`git log --all --diff-filter=A -- '**/known-issues.md'`'s single hit, which
predates this comment. The gate in `src/docs.rs` only resolves
`issues/<area>/<slug>.md` paths, so a citation to the file that directory
replaced is invisible to it — this one was found by a tree-wide grep for
document paths that do not resolve, run while closing an unrelated
restructure (#39).

Found in passing; not fixed here to keep that PR to moves and reference fixes.
The fix is finding what "§4" pointed at — most likely the disk-wait/spinlock
chain in `issues/audio/disk-wait-pins-a-cpu.md`, since that is root
`CLAUDE.md`'s current name for the T14 audio-pop mechanism — and repointing the
comment at it, or at whichever `issues/` entry actually carries it.
