---
status: open
kind: finding
opened: 2026-08-13
---

# A kernel doc comment cites `specs/known-issues.md`, which stopped existing before this branch

`kernel/src/drivers/xhci/mod.rs:359` (added `17a6b17`, 2026-08-08): "*... and
that is the whole of what the T14's audio pops are made of
(`specs/known-issues.md` §4).*" `specs/known-issues.md` is the monolithic file
`specs/issues/` replaced (`specs/issues/README.md`'s `opened` field still
explains the split); it does not exist on any branch found by
`git log --all --diff-filter=A -- '**/known-issues.md'`'s single hit, which
predates this comment. The gate in `src/docs.rs` only resolves
`specs/issues/<area>/<slug>.md` paths, so a citation to the file that directory
replaced is invisible to it — this one was found by a tree-wide grep for
`specs/` paths that do not resolve, run while closing an unrelated specs
restructure (#39).

Found in passing; not fixed here to keep that PR to moves and reference fixes.
The fix is finding what "§4" pointed at — most likely the disk-wait/spinlock
chain in `specs/issues/audio/disk-wait-pins-a-cpu.md`, since that is root
`CLAUDE.md`'s current name for the T14 audio-pop mechanism — and repointing the
comment at it, or at whichever `specs/issues/` entry actually carries it.
