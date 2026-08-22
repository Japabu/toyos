---
status: none
kind: rejected
opened: 2026-08-22
---

# Normalizing a measured price by the shard's boot width is refused by the data

`tests/common/qemu.rs`'s `host_scale` divides the run's fastest boot by a 1320 ms
reference and multiplies every liveness ceiling by the result; every shard prints
it (`host: fastest boot N ms against the reference 1320 ms — liveness ceilings
paid at Wx width`). The proposal was to spend the same factor on the *duration
profile*: divide each shard's measured prices by its width in
`src/durations.rs`'s merge, so `src/tiers.rs`'s ceiling compares like with like
across shards of different speed, with timer-anchored names exempt because a
fixed wait does not shrink on a fast host.

**Measured over six hosted twelve-shard runs and refused.** Dividing by the
width makes the profile 77% *worse*, and on the lane that renders the verdict
the width carries no signal at all.

## The measurement

Six GitHub-hosted twelve-shard runs of `ci.yml`, 72 shard-runs, each shard's
width read from its own `host:` line and its prices from its
`test-durations.shard-<i>-of-12` artifact:

| run | event | shard widths |
|---|---|---|
| 32550410305 | merge queue, 03:57Z | 1.24x – 2.74x |
| 32549550794 | merge queue, 03:39Z | 1.33x – 2.52x |
| 32548181023 | merge queue, 03:09Z | 1.10x – 2.46x |
| 32549405777 | nightly, 03:35Z | 1.23x – 3.55x |
| 32444411794 | nightly, 2026-08-21 | 1.11x – 2.54x |
| 32329029347 | nightly, 2026-08-20 | 1.14x – 3.46x |

640 observations over the 130 names priced at or above 1,000 ms and seen in at
least three of the six. Because the partition is a function of the committed
profile, a name lands on the same shard index in every run of one tier, so the
same name is priced at several different widths across runs.

**A per-shard common price factor is real, and it is small.** Fitting
`ln(price) = name + shard-run` two ways drops the within-name residual sd from
0.1242 to 0.0816 — a per-shard factor explains 56.9% of the run-to-run variance
a name shows. But that factor's own spread is sd 0.097 in logs, a p10–p90 range
of about 1.28x, while the widths span 1.10x to 3.55x (sd 0.272 in logs).

**The width does not measure it.** Regressing the fitted per-shard factor on
ln(width): slope **0.159**, R² **0.198** over the 72 cells. Split by lane:

| cells | slope | R² | sd of the true factor |
|---|---|---|---|
| the three merge-queue runs (36) | **0.014** | **0.003** | 0.069 |
| the three nightlies (36) | 0.256 | 0.373 | 0.124 |

The merge queue is where the tier verdict is rendered, and there the width
explains 0.3% of the price factor. The reverse regression is 0.180 there, so it
is not attenuation — the two are unrelated on that lane. Pooled over all six the
reverse regression is 1.248 with `var(ln width)` 7.86x `var(factor)`: even read
charitably, ln(width) is the shared cause buried under eight times its own
variance of noise, and dividing by it adds that noise to every price.

**Within-name slope of ln(price) on ln(width), by `src/tiers.rs` relegation
class** — the prediction was ~1 for ordinary and `Why::Cost` names and ~0 for
`Why::TimerAnchored`:

| class | names | obs | slope | R² |
|---|---|---|---|---|
| Fast (unrelegated) | 83 | 493 | 0.201 ± 0.026 | 0.128 |
| `Why::Cost` | 21 | 66 | 0.127 ± 0.052 | 0.118 |
| `Why::TimerAnchored` | 25 | 78 | 0.233 ± 0.056 | 0.248 |
| `Why::RidesTheBootOf` | 1 | 3 | 0.215 ± 0.448 | 0.186 |

Both halves of the prediction fail, and they fail in opposite directions: no
class is near 1, and `TimerAnchored` — the class that was supposed to be flat —
is the *steepest* of the four. On the merge-queue runs alone the Fast slope is
−0.033 ± 0.032. The classes are not distinguishable by this factor.

## What the normalization would have done

Within-name sd of `ln(price − k·ln width)`, over the same 640 observations:

| k | sd | of raw |
|---|---|---|
| 0.00 (as committed today) | 0.1242 | 100% |
| 0.20 (the best-fit correction) | 0.1154 | 92.8% |
| 0.50 | 0.1351 | 108.7% |
| **1.00 (the proposal)** | **0.2196** | **176.7%** |

The best linear correction available buys 7% and is not worth a mechanism; the
proposal costs 77%.

It would also disable the rule it was meant to serve. Of the 13 Fast-name
observations over PR #200's `FAST_COMMIT_MS` of 8,000 ms in these six runs,
**zero** survive the division. `i8042_health` is the clearest case: over the line
in five of the six runs, at widths 1.14x, 1.51x, 1.83x, 1.93x and 2.50x — no
relation to the width at all — and normalized it would read 7,957, 5,716, 5,202,
4,892 and 3,735 ms, a 2.1x swing manufactured entirely out of boot noise.

## The finding that started this, corrected

Run 32550410305's `durations` job refused `xhci_full_speed_device` at 9,890 ms.
That measurement came from **shard 5, whose width was 1.68x** — not from the
1.43x shard 2 whose `host:` line was read beside it. `6900 × 1.43 = 9867` was a
coincidence.

The name's six measurements are 4,700 (1.37x), 6,816 (1.33x), 6,900 (2.54x),
7,456 (2.38x), 7,499 (1.10x) and 9,890 (1.68x) ms: its two slowest shards
produced its second- and third-*cheapest* prices. Its own within-name sd of
ln(price) is 0.219 against a population 0.124 — the 9th most variable of the 83
Fast names measured in five or more runs, behind `diskless_boot` (0.321),
`control_regs_negative` (0.317), `nvme_wide_sector` (0.300), `control_regs`
(0.261), `virtio_used_ring` (0.232), `console_line_atomicity` (0.231),
`foreign_disk_untouched` (0.230) and `screen_panic_muted` (0.227). Shard 5's own
fitted factor that run was 1.093, so 9% of the 40% excess over the name's
geometric mean was the shard and the rest was the test.

**So the name is priced without margin because it is a variable test**, which is
what PR #200's refusal says and what its remedy — relegate it or make it faster
— addresses. Nothing about the shard's speed is in the way.

## What the committed profile's width turned out to be

`tests/test-durations` on `main` is byte-identical to run 32444411794's
`test-durations-merged`, so the profile is exactly that nightly. Its twelve
shards ran at 1.11x–2.54x width, median 1.61x — and their fitted price factors
were 0.918–1.317, median 0.957, against a mean of 1.0 over all 72 cells. The
profile is already within a few percent of reference price, and its 2.3x spread
of *width* corresponds to no meaningful spread of *price*. There is nothing to
renormalize.

## What is still true and is not this

`issues/build/the-duration-profile-is-enforced-where-it-was-not-measured.md` is
untouched by any of the above: it is about two *machines* — twelve hosted EPYC
shards against one T14 lane — and its measurement is a 1.35–1.37x gap between
them on an idle host, not a within-lane shard factor. This file says only that
the boot-width number cannot stand in for that, or for anything else in a price.

Rejected on measurement, 2026-08-22, by the task that was sent to build it.
