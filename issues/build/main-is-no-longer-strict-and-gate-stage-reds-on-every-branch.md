---
status: owner
kind: question
opened: 2026-08-19
---

# `main`'s required checks stopped being strict, so `gate-stage` reds on every branch

`landing.yml`'s `gate-stage` reads main's ruleset back out of GitHub and refuses
a repository whose required checks are not strict:

```
##[error]required checks are not strict, so a branch could merge without
##[error]containing main and its checks would not be about the merged result
```

`strict_required_status_checks_policy` is `false` in ruleset `20589156` as of
2026-08-19. The five required contexts are all still there — `host`,
`abi-split`, `gate-stage`, `guest-suite`, `build` — and every other clause the
job checks still holds: `deletion`, `non_fast_forward`, `pull_request`,
`required_status_checks`, and `merge` as the only allowed method.

**It is repository-wide and it is dated.** `gh run list --workflow landing.yml`
on 2026-08-19:

```
13:52  failure  wt/toyos-clippygate
13:52  failure  wt/toyos-lifecycle
13:50  failure  wt/toyos-adopt
13:50  failure  wt/toyos-untrusted
13:44  failure  wt/toyos-lifecycle
13:43  failure  wt/toyos-p90measure
13:43  failure  wt/toyos-batch1
13:42  failure  wt/toyos-i8042attr
13:41  failure  wt/toyos-clippygate
13:40  success  main
13:39  success  wt/toyos-clippygate
```

Seven worktrees, and the same branch green at 13:39 and red at 13:41 with two
commits between them that touch neither `landing.yml` nor anything it reads. The
setting moved between 13:40 and 13:41; nothing in VCS did.

`gate-stage` is itself one of the five required checks, so while this holds
**no branch in this repository can merge**, whatever else is green on it.

## The decision, which is the owner's

Two things could be true and only he knows which:

- **Strict was turned off by accident**, or by something else touching the
  ruleset, and turning it back on is the whole fix.
- **Strict was turned off on purpose** — it is what makes every branch re-run
  its checks after each merge into `main`, which on this repository means a
  toolchain bootstrap and twelve guest shards per landing. Then `gate-stage`'s
  claim is what is wrong, and the paragraph it prints — *"Strict is what makes a
  check on this head a check on the merged result — the property `--land`'s `git
  merge --no-ff main` used to carry"* — has to be replaced by whatever now
  carries that property, or by an explicit statement that nothing does.

An agent must not decide this: the first reading is a click, the second is a
change to what "green" means on every pull request this repository will ever
have.
