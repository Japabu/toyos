---
status: open
kind: track
opened: 2026-08-20
---

# The eased merge law carries a threshold

An external review (2026-08-20, adopted by the owner) named the eased law for
what it is: a deliberate correctness/throughput trade, not an equivalent of
pre-merge composition testing. A branch green against an older `main` can
merge after other changes and create a composition that was never tested
before becoming authoritative. Platform constraints explain the choice; they
do not demonstrate it is cheap. So the trade is instrumented, and the
threshold and consequence are defined now, not after the first incident.

**The instrument measures, from CI history, on a fixed cadence:**

- post-merge red-`main` incidents (the push-triggered run on `main`'s tip);
- total and p95 time `main` spends red;
- merges landing before validation of the previous tip completed;
- failures caused by interaction between PRs that were independently green
  (the incident class strict testing would have caught).

**Threshold and consequence, defined now:** the expected rate is near zero.
One interaction-failure incident, or more than one red-`main` incident in a
rolling week, and the stronger serialization returns — batch landings under
the orchestrator, or the organization move that unlocks GitHub's merge queue
— as a mandatory response, not an aspiration. CLAUDE.md's landing bullet
points here.

**The three defense layers stay distinct**, per the same review, and none
substitutes for the others: (1) pre-merge composition testing (what the ease
traded away, what the threshold guards); (2) independent oracles (the
high-risk-change rule in CLAUDE.md); (3) long-horizon empirical testing —
boot storms, fault injection, sighting correlation, hardware observation
(the redlist's practice). Measure them separately; report them separately.

## 2026-08-20 report — the first window, and the threshold is already breached

**When the ease actually took effect.** Not the CLAUDE.md commit (`a7f7716`,
2026-08-19T23:13:22Z) — that is prose. The enforcement change is the branch
protection ruleset itself: `gh api repos/Japabu/toyos/rulesets/20589156/history`
gives `strict_required_status_checks_policy` flipping to `false` at
`2026-08-20T01:14:24+02:00` = **2026-08-19T23:14:24Z**, the owner's own local
clock, which is what "2026-08-20T01:14Z" in this track's brief was reading off.
Every number below is windowed from that instant to
`2026-08-20T12:10:44Z` (this report's snapshot), `gh run list --branch main
--event push --created ">=2026-08-19T23:14:24Z"`, 12h56m elapsed.

**Total pushes to `main`: 27** (26 concluded, 1 still validating at snapshot
time), each producing four push-triggered runs — `ci` (`guest-suite`),
`host tests` (`host`), `toolchain` (`build`), `landing` (`gate-stage`;
`abi-split` is a no-op on a push event) — 108 runs total, confirmed a clean
4-way partition per push (`108 = 27 × 4`).

**Red-`main` incidents: 4 of 27 pushes (14.8 %) — the threshold's ">1 in a
rolling week" line is already crossed, 12h56m in.**

| when (UTC) | headSha | check red | run | what |
|---|---|---|---|---|
| 23:15:57 | `8b1d0c19` | `gate-stage` | [32312506182](https://github.com/Japabu/toyos/actions/runs/32312506182) | rollout artifact — the ruleset flipped 1m33s before this push's `gate-stage` script (still the pre-ease one) ran; the fix landed 6m10s later in `0bd030fe` (#141) |
| 23:15:57 | `8b1d0c19` | `guest-suite` | [32312506258](https://github.com/Japabu/toyos/actions/runs/32312506258) | `screen_console_shell`, already known-red (`src/redlist.rs`), `ALONE: GREEN` |
| 23:39:26 | `eba06ad6` (#140) | `guest-suite` | [32314166262](https://github.com/Japabu/toyos/actions/runs/32314166262) | `console_locale_detect`, **first sighting** — filed this session, `issues/build/parallel-tests-red-under-other-suites.md` and a new `src/redlist.rs` row, `ALONE: GREEN` |
| 00:14:54 | `8a6cbf43` (#144) | `guest-suite` | [32316558154](https://github.com/Japabu/toyos/actions/runs/32316558154) | `screen_console_panic` + `tlb_shootdown_waits`, both already known-red, both `ALONE: GREEN` |
| 04:07:38 | `0a7470fa` (#132) | `guest-suite` | [32330716400](https://github.com/Japabu/toyos/actions/runs/32330716400) | `tlb_shootdown_waits` again, same shard position as the row above, already known-red, `ALONE: GREEN` |

**Interaction failures (independently-green PRs composing into a broken
result): 0.** Every red above is a single-test, single-shard, host-load-shaped
flake — `ALONE: GREEN` on the harness's own re-run, four of five already
carried a `src/redlist.rs` row before this backfill. None traces to what two
merged branches did *together*; the one non-flake row (`gate-stage`) is the
ease's own rollout sequencing, not a composition defect. So the specific
failure mode the ease was named for — a tested-green branch merging into a
tested-green `main` and the *pair* being broken — has not yet been observed.
The rate rule breaches anyway, on volume of ordinary flakiness alone.

**Total and p95 time `main`'s tip spent red.** Grouping the five row-level
reds above into continuous intervals (an interval ends when the same required
check next reports green on a later tip):

| interval | check | red at | green at | run that recovered it | minutes |
|---|---|---|---|---|---|
| 1 | `gate-stage` | 23:16:08 | 23:23:10 | `0bd030fe` landing, [32312941345](https://github.com/Japabu/toyos/actions/runs/32312941345) | 7.0 |
| 2 | `guest-suite` | 23:33:59 | 23:44:23 | `fd8b1af` ci, [32313554473](https://github.com/Japabu/toyos/actions/runs/32313554473) | 10.4 |
| 3 | `guest-suite` | 23:54:59 | 00:19:16 | `a5ccf14` ci, [32316308871](https://github.com/Japabu/toyos/actions/runs/32316308871) | 24.3 |
| 4 | `guest-suite` | 00:29:46 | 05:27:09 | `df99edb` ci (#149), [32334556527](https://github.com/Japabu/toyos/actions/runs/32334556527) | **297.4** |

Interval 4 spans two red pushes (`8a6cbf43` then `0a7470fa`) with no green
`guest-suite` run on `main`'s tip in between — the tip was red for **4h57m**
straight, closed only when PR #149 (the scheduler-steal-race fix, an unrelated
kernel change) happened to land next. **Total: 339.1 minutes (5.65 h). p95
(nearest-rank, n=4): 297.4 minutes** — one incident dominates both numbers,
which is what small-N p95 does; read the total alongside it. Red time is
**43.7 % of the window's 776 elapsed minutes**, entirely attributable to one
long stretch where nobody was watching `guest-suite`'s own required check
between two pushes that each separately reintroduced the same known flake.

**Merges landing before the previous tip's validation completed: 8 of 27
pushes (29.6 %).** `host-tests.yml` carries `cancel-in-progress: true`
unconditionally and `ci.yml`/`landing.yml` auto-supersede a same-group *queued*
run regardless of their own `cancel-in-progress` setting — so a push whose
predecessor's required-check run had not yet started gets that predecessor's
run cancelled outright, `jobs: []`, never validated at all. Pushes with at
least one required-check run cancelled this way: `0bd030fe`, `3015907f`,
`ed042259`, `a5ccf14`, `4228c9fa`, `7fe85f38`, `68c87f47`, `64130c45` — three
of those eight (`3015907f`, `ed042259`) had *every* required check cancelled,
meaning that main tip received zero push-triggered validation of any kind
before the next push superseded it. This is the composition-testing gap the
ease trades away, made concrete: these tips were never checked, not even
after the fact.

**Verdict: THRESHOLD BREACHED**, on red-`main` incident count alone (4 > 1 in
under 13 hours, not a week) — the interaction-failure count is 0, so the
*specific* risk named in the review has not fired, but the track's own rule
does not carve out an exception for "the reds were all pre-existing flakes."
Per this file's own consequence line, the stronger serialization is now the
mandatory response: batch landings under the orchestrator, or the
organization move that unlocks GitHub's merge queue. That is a call for the
owner, not this instrument — this entry records the measurement and the rule
it triggers, and does not itself change the landing protocol.

`cargo run -- --merge-health` (`src/mergehealth.rs`) recomputes these same
numbers over a rolling window on demand and is wired into `ci.yml`'s nightly
schedule as a reporting step, so this verdict is re-checked automatically
rather than resting on this one dated snapshot.
