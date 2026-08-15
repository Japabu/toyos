---
status: open
kind: defect
opened: 2026-08-15
---

# The shard split prices a task's boot and not the image it builds first

`Shard::keep` partitions the twelve shards by `tests/test-durations`, and that
profile records what a *test* took — the number the ten-second Fast ceiling is
also read against. A machine test that boots a config the shared image does not
cover builds that image first, and nothing prices that build at all.

**Measured on run `31896922288`** (`main` at `e064a96`, twelve KVM shards, the
Fast tier), reading each shard's job log for the wall clock between one `PASS`
and the next:

| task | the gap before it | what the profile says it costs |
|---|---|---|
| `boot_partition_identity` (shard 7, `tests/metalcase`) | **197.8 s** | 5,511 ms |
| `sshd_fail_closed` (shard 4, `tests/sshdcase`) | **145.3 s** | 2,495 ms |
| `desktop_locale_detect` (shard 8, `tests/desktopcase`) | 47.4 s | 3,939 ms |
| `sched_check_build` (shard 8, the `sched-check` kernel) | 32.4 s | 5,879 ms |

The two large ones are configs carrying userland programs the shared test image
does not: `metalcase` starts `compositor`, `netd` and `sshd`, `sshdcase` starts
`netd` and `sshd`, and every one of our crates recompiles in a fresh checkout
(`specs/assessments/ci-plan-assessment-2026-08.md` §12.5 says why, and that the
direction is deliberate).

**What it costs the partition.** The twelve `suite` steps of that run:

| shard | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 | 10 | 11 | 12 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| s | 164 | 164 | 171 | **313** | 167 | 160 | **347** | 235 | 159 | 163 | 153 | 170 |

Sum 2,366 s, an even split of 197.2 s, a widest shard of 347 s. Shard 7 is
115.9 s of floor plus one 203.8 s slot the profile priced at 5.5 s. LPT cannot
place what it cannot see: every shard shows `6 parallel task(s)` because the
partition believes it is splitting six 3-to-7-second tests.

**And it believes it succeeded.** Summing each shard's own tests at their
committed prices, that run's twelve bins hold **30.2 s to 32.5 s** — a spread of
2.3 s. The clock's spread over the same twelve is **194 s**. Nothing in the
profile is wrong; the profile is simply not what a shard's wall clock is made
of.

**The A/B that shows the two objectives diverging.** Runs `31900045901` (`main`
at `e064a96`) and `31900050723` (the same tree plus the one-accumulator fix),
both twelve-shard `--nightly` dispatches, minutes apart on the same runner pool:

| | widest priced bin | priced spread | widest phase total |
|---|---|---|---|
| before | 471.6 s | 328.7 s | 369.8 s |
| after | **324.7 s** | **179.0 s** | **380.8 s** |

The partition got 147.0 s better by the number it optimises and 11.0 s worse on
the clock — inside that pair of runs' own noise, but not an improvement. Both
runs place the identical 316 names with no duplicate, and both sum to 2,180.6 s
priced, so this is one partition against another over the same work.

**The bound a correct price would reach is not the even split either.** The
image build is indivisible and attached to its task, so the widest shard is at
best floor + 203.8 + its own test ≈ 316 s against today's 347 s — about 31 s,
and the rest of the 150 s over the even split is the build itself rather than
where it landed.

Two directions, and they are not exclusive:

- **Price a task by build + boot.** The profile cannot simply absorb it: the
  same file is what the `durations` verdict reads against the 10,000 ms Fast
  ceiling, so a task priced at 203 s would red the tier gate it has nothing to
  do with. It wants a second profile — per *config*, not per test — that only
  `Shard::keep` reads.
- **Make the second image cheap.** 145 s to add `netd` and `sshd` to an image
  is a full recompile of those programs, not a relink.

Filed from the CI wall-clock task of 2026-08-15, which measured it while
landing the one-accumulator fix and did not touch it.
