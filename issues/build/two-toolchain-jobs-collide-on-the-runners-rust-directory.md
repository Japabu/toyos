---
status: open
kind: defect
opened: 2026-08-22
---

# Two `toolchain.yml` builds in the same workspace abort on `rust/`

`toolchain.yml`'s `build` job runs `git submodule update --init rust` in the
runner's workspace. On run `32549542807` (PR #209, 2026-08-22 03:38:48Z) it
failed before compiling anything:

```
Submodule 'rust' (https://github.com/ToyOSOrg/rust.git) registered for path 'rust'
fatal: destination path '/home/t14/actions-runner/_work/ToyOS/ToyOS/rust' already exists and is not an empty directory.
fatal: clone of 'https://github.com/ToyOSOrg/rust.git' into submodule path '.../rust' failed
Failed to clone 'rust'. Retry scheduled
fatal: destination path ... already exists and is not an empty directory.
Failed to clone 'rust' a second time, aborting
##[error]Process completed with exit code 1.
```

Nine seconds, and no line of the tree was read. The blast radius is the whole
gate: `toolchain-ready` then polled for eight minutes and sixteen seconds and
ended with `the toolchain for this tree did not build, so the guest shards would
have waited two hours to install nothing`, and `guest-suite` failed in three
seconds behind it. Three red required checks on a pull request whose diff the
runner never compiled.

## Why it is not the tree

The neighbouring runs of the same workflow on the same day are green:
`32549437553` (`wt/toyos-mhregime`, 03:36:28Z) and `32549630106`
(`wt/toyos-returnrule`, 03:40:41Z) both succeeded, two minutes either side of
the failure. `gh run list --workflow=toolchain.yml --limit 12` is where those
three lines are. The failing branch's own re-run is the other half of the
evidence and is the reason this is filed rather than fixed.

`actions/checkout` does not remove an uninitialised submodule's directory, so a
workspace that already holds a populated `rust/` — from a job that was still
running, or one that was cancelled between the clone and the checkout — makes
the next `git submodule update --init rust` in that workspace abort rather than
adopt what is there.

## What is not known

Whether the two overlapping jobs were on **one** worker or two.
`issues/build/the-t14-has-one-lane-and-the-nightly-wants-three.md` records
`gh api repos/ToyOSOrg/ToyOS/actions/runners` reporting one runner with one
worker as of 2026-08-21, and strictly serial jobs cannot collide this way — so
either that reading has moved, or a cancelled job left the directory behind and
the collision is with a corpse rather than with a neighbour. The runner's own
`_work` directory is where that is settled, and nothing in the repository can
answer it.

## What would fix it

Not a retry: the second attempt in the log above is git's own and it failed the
same way. Either the step adopts an existing directory (`git submodule update
--init --force`, or removing `rust/` before the clone when it is not already a
gitlink), or the job stops sharing a workspace with another run of itself
(`concurrency:` on the workflow, keyed on the runner rather than on the branch).
Both are decisions about `.github/workflows/toolchain.yml`, which this task does
not own.
