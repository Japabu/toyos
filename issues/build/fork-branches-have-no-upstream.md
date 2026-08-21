---
status: open
kind: finding
opened: 2026-08-07
---

# A `toyos` branch mostly has no upstream, so `git status` cannot say if it is pushed

13 of the 16 consumed and PR branches across `forks/` have no tracking ref:
`git for-each-ref --format='%(refname:short)|%(upstream:short)' refs/heads` gives
`NO UPSTREAM` for cpal, ctrlc, getrandom (all three), mio, raw-window-handle,
socket2, softbuffer, stacker, target-lexicon, tokio and winit. Only libloading,
memmap2 and russh track `origin/toyos`, and target-lexicon's `add-toyos-os`
tracks `upstream/main` — which is why it reads `ahead 1` rather than in sync.

The consequence is that `git status` on a fork's `toyos` branch prints `## toyos`
and nothing else: the ordinary way to ask "have I pushed this?" is silently
unanswerable in the clones where the answer matters. Every one of them happened
to be in sync on 2026-08-07, established by comparing `rev-parse HEAD` against
`rev-parse origin/<branch>` rather than by reading `git status`. One
`git branch -u origin/<branch> <branch>` per clone fixes it; outside the repo,
so the owner's hands.
