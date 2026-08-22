---
status: open
kind: finding
opened: 2026-08-21
---

# The T14 has one lane and the nightly wants three

`gh api repos/ToyOSOrg/ToyOS/actions/runners` reports one runner with one
worker, so every job routed to it runs strictly after the last. On a pull
request that is fine — `ci.yml`'s guest lane is the only long trusted job and
`toolchain.yml`'s build exits in seconds unless the sysroot moved. The nightly
schedule is where it has never been measured:

- `ci.yml` at 03:00 runs `guest` with `--nightly` as a 1/1 partition and `tcg`
  after it;
- `gate-a.yml` at 03:10 asks for up to 180 minutes of the same machine.

Both queue on one lane. Nothing breaks — queue time is not a job's
`timeout-minutes` — but gate A's nightly sample is taken hours later than its
cron says, and `guest`'s own 120-minute ceiling was chosen against the Fast
tier, not the widened one.

The numbers this rests on today: the hosted twelve-way partition's shards took
4–8 minutes each on run 32413874545, and `tests/test-durations` sums to
2,153,613 ms of test time. A 1/1 lane pays setup once and that test time in
full, so an ordinary run is roughly 45 minutes and a nightly is unmeasured.

What settles it is the first nightly on the T14: if `guest` reds on its ceiling
or gate A's sample lands after the working day starts, the ceiling or the
routing has to move. Until then this is a shape nobody has watched, not a
defect anybody has seen.

## 2026-08-22: it was not the nightly, it was the branches — the routing moved

The queue nobody had watched arrived from the other direction. Measured at
05:03Z with `gh run list --status queued`:

| queued since | workflow | event | branch |
|---|---|---|---|
| 03:56:59Z | `toolchain` | push | `main` |
| 03:58:35Z | `toolchain` | pull_request | `wt/toyos-hwswitch` |
| 03:59:54Z | `toolchain` | pull_request | `wt/toyos-dfkvm` |
| 04:00:39Z | `toolchain` | pull_request | `wt/toyos-census-gate` |
| 04:16:31Z | `toolchain` | pull_request | `wt/toyos-book3` |
| 04:17:44Z | `toolchain` | pull_request | `wt/toyos-dfsweep` |
| 04:26:35Z | `ci` | push | `main` |
| 04:40:20Z | `ci` | pull_request | `wt/toyos-durwidth` |
| 04:44:52Z | `toolchain` | pull_request | `wt/toyos-unsafedrv` |
| 04:49:31Z | `ci` | pull_request | `wt/toyos-unsaferoot` |
| 04:55:42Z | `ci` | pull_request | `wt/toyos-unsafedrv` |
| 04:58:58Z | `ci` | pull_request | `wt/toyos-dfsweep` |
| 05:03:25Z | `landing` | pull_request | `wt/toyos-book3` |

Thirteen runs, all of them this repository's own branch traffic, behind one
scheduled `gate A, thorough` that had held the lane since 03:53Z. Seven are
`toolchain`, and a `toolchain` run that has nothing to build exits in seconds —
they were waiting, not working: three of them (32531080726, 32531085125,
32531090861) measured 34.1, 34.4 and 34.5 minutes wall clock the night before,
started 8 seconds apart and finished 30 seconds apart — a lane draining, not
three builds. `toolchain.yml`'s `build` is a required check and `ci.yml`'s
`toolchain-ready` polls up to two hours for what it publishes, so this queue is
what a pull request waits on. Two agents waited 50 and 90 minutes for their own
validation.

**The decision (owner's orchestrator, 2026-08-22): branch traffic is hosted,
and the T14 serves what needs the machine.** `route.yml`'s `HOSTED` now covers
`merge_group`, `pull_request` (from anywhere), `push`, and `ci.yml`'s
`schedule`. What is left on the T14 is every other `schedule` — gate A's audio
measurement and portability — and `workflow_dispatch`. Nothing was added to the
machine and no job was created for it: the persistent build cache keeps being
written in place by whatever trusted job runs (`link-build-cache.sh`), which
after this is the nightly gate A on `main`'s tip and any dispatch.

The trade, stated: a hosted lane is twelve-wide and unqueued — ten merge-queue
`ci` runs on 2026-08-21/22 finished in 10.5 to 17.5 minutes — but a *real*
toolchain bootstrap now happens on a hosted runner rather than on the laptop's
eight cores, and nothing on record measures one there (every hosted `toolchain`
run to date found the release already published and exited in 0.2–0.9 min).
`ci.yml`'s `toolchain-ready` gives it two hours and `toolchain.yml` itself 350
minutes. **The next sysroot-changing pull request is what measures that**, and
if a hosted bootstrap does not fit in two hours the ceiling or the bootstrap's
routing is what moves — not the branch lanes.

### What the queue was hiding: the shared work area poisons `rust/`

Found while measuring the above, on the machine itself. A self-hosted runner
gives every job in a repository the same work area, and `rust/` is a tracked
gitlink: `actions/checkout` neither populates it nor cleans it (`git clean`
does not touch a tracked path), so it is whatever the last job left.
`.github/install-toolchain.sh` does `mkdir -p rust` and hangs the persistent
cache's toolchain entry off `rust/build` — in every trusted guest, gate-A and
probe job — so any of them leaves a `rust/` holding one symlink and no `.git`.
`git submodule update --init rust` then refuses it (`destination path 'rust'
already exists and is not an empty directory`) and **every** toolchain job on
the machine fails nine seconds in until somebody logs in.

Measured 2026-08-22: one entry, `build ->
/toyos-cache/toolchains/toolchain-linux-x86_64-a4ad5f0abfe8a8e2`, 8 KB, created
04:53:43Z; run 32549542807 failed on it twice while `toolchain-ready` waited 57
minutes for a release that was never coming. It had only ever worked because a
valid checkout from an earlier job happened to survive. The second shape is
worse and had not fired yet: a *valid* checkout carrying that same `rust/build`
link would have the bootstrap write the fork's build tree straight into the
cached toolchain entry, and `publish` would tar it back out.

`toolchain.yml`'s `the rust fork` step now tells the shapes apart — it prints
what it found, removes a `rust/` that is not a checkout git can use, unlinks a
`rust/build` symlink inside one that is, and never follows the link. Seven
shapes were driven against the step's own script before it landed. The routing
change above removes the recurrence for branch traffic by itself; the step is
what covers the dispatch lane that stays.
