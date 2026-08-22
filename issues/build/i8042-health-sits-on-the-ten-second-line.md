---
status: open
kind: defect
opened: 2026-08-21
---

# `i8042_health` reds the `durations` job from either side of the ten-second line

`i8042_health` returned to `Tier::Fast` on 2026-08-21 (`src/tiers.rs`'s own note)
on nightly run `32444411794`'s measurement of **9,509 ms**, 5% under the 10,000 ms
line `src/durations.rs` enforces. The next measurement of it went over:

    the merged CI profile and tier declaration disagree:
    i8042_health measured 10281 ms in CI, over the 10000 ms line, but
    i8042_health remains Fast

— pull request #197's `durations` job, run `32506320411`, job `96850003410`,
2026-08-21 17:16 UTC. `guest partition: success` in the same run: the suite
passed and only the profile disagreed, so `guest-suite` reds on a green suite.

`tests/test-durations` carries `i8042_health 9509`. The two measurements are
8% apart and the line is between them, so the classification is decided by
which side of a coin the shard lands on — and the red lands on whatever pull
request measured it next, which is neither its author's diff nor a defect in
the tree. #197's diff is soundd's stats line and a mixer counter; it touches
nothing i8042 does and nothing that could cost it 772 ms.

`cargo run -- --known-red i8042_health` answers `NOT ON THE LIST`, so nothing
adjudicates it today and every author who meets it re-derives the above.

Whoever takes it has three options and they are not equivalent: relegate it to
`Nightly` with an honest `Why::Cost` row (which is where it came from, at
47,121 ms, before the 2026-08-19 i8042 pacing fix); find the 772 ms and remove
it; or decide that a name whose CI cost straddles the line needs a hysteresis
the profile does not have today. The measurement to take first is its variance —
two points cannot say whether 9,509 and 10,281 are one population or a
regression between them.

**The first option was taken and the variance was measured, 2026-08-22.** PR
#200's margin sweep relegated this name with `Why::Cost` and the guards row
`src/tiers.rs` now carries; `tests/test-durations` still prices it 9,509 ms, so
its row sits in the band on purpose and the return rule holds it there. PR
#214's measurement
(`issues/build/a-shards-boot-width-does-not-price-its-tests.md`) is the variance
this file asked for: over six hosted twelve-shard runs it was over
`FAST_COMMIT_MS` in **five of the six**, at widths 1.14x to 2.50x with no
relation to the width — the only one of thirteen over-the-line Fast observations
in those runs that is over it consistently rather than by shard luck. So this is
a population and not a coin: 9,509 and 10,281 are two draws from a distribution
whose bulk is at or above the line, and `Why::Cost` is the honest reading. What
is still owed is the second option — the 772 ms, and now the rest of it.
`cargo run -- --known-red i8042_health` still answers `NOT ON THE LIST`.
