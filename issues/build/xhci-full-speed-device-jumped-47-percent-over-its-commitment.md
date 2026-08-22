---
status: open
kind: defect
opened: 2026-08-21
---

# `xhci_full_speed_device` is a variable test, and no tier holds one

`tests/test-durations` carries `xhci_full_speed_device 6900`. Pull request
#199's `durations` job, run `32513441183`, measured it at **10,166 ms** and
reded the gate:

    xhci_full_speed_device measured 10166 ms in CI, over the 10000 ms line,
    but xhci_full_speed_device remains Fast

Merge-queue composition `32550410305` then measured it at **9,890 ms** and reded
the same job from the other side of `FAST_COMMIT_MS`:

    xhci_full_speed_device is priced at 9890 ms — over the 8000 ms a Fast test
    may be committed at and under the 10000 ms line — and xhci_full_speed_device
    remains Fast: priced without margin, so relegate it or make it faster.

**This is not the straddle the margin rule covers, and the rule says so.**
`src/tiers.rs`'s `FAST_COMMIT_MS` refuses a `Tier::Fast` name priced in
`(8000, 10000]` because such a price is decided by which partition ran it. This
name has margin — 6,900 ms is 31% under the commitment line — and crossed the
ceiling anyway, which is a 47% jump over the price it is committed at. The
derivation of the fifth is exactly that a Fast name over the ceiling *has* to
have grown by at least a quarter, so its red is a finding about the test rather
than a coin landing. This is that finding, and nothing in the margin sweep
addresses it.

The test is compute-bound by construction — `tests/common/usb.rs`'s
`xhci_full_speed_device` boots one machine, shuts it down, and asserts on
substrings of the resulting log; there is no sleep, no deadline and no rate in
it — so it is neither `Why::TimerAnchored` nor a candidate for one. Nor can it
be relegated as `Why::Cost` at its committed price: the return rule would
immediately refuse that row ("every current CI label is at or under the 8000 ms
commitment line and it belongs Fast"). It is `Tier::Fast` and it must either
stay under 8,000 ms or be shown to cost more than it is committed at.

## The variance, measured

The measurement this file asked for was taken by PR #214 and is written down in
`issues/build/a-shards-boot-width-does-not-price-its-tests.md`: six hosted
twelve-shard runs, 72 shard-runs, 640 observations over the 130 names priced at
or above 1,000 ms and seen in at least three of the six.

**Neither of this file's two readings is right.** The name's six prices are

| ms | shard width |
|---|---|
| 4,700 | 1.37x |
| 6,816 | 1.33x |
| 6,900 | 2.54x |
| 7,456 | 2.38x |
| 7,499 | 1.10x |
| 9,890 | 1.68x |

— its two slowest shards produced its second- and third-*cheapest* prices, so
this is not co-scheduling read off the boot width, and the series does not shift
in one direction over the window, so it is not a regression between two dates
either. It is the *test's own* spread: within-name sd of `ln(price)` **0.219**
against a population **0.124**, the **9th most variable of the 83 Fast names**
measured in five or more runs. Shard 5's fitted price factor in run 32550410305
was 1.093, so 9% of that reading's 40% excess over the name's geometric mean was
the shard and the rest was the test.

That measurement also refuses the obvious mechanism: a per-shard common price
factor is real — it explains 56.9% of the run-to-run variance a name shows — but
its own spread is only about **1.28x p10 to p90**, and it is **not** the shard's
boot width (slope 0.014, R² 0.003 over the merge-queue lane's 36 cells). There
is nothing to divide out.

## The answer: the nightly decides it, and this name stays Fast

**The orchestrator's ruling, 2026-08-22.** `xhci_full_speed_device` remains
`Tier::Fast` and is not relegated. A variable name has no stable tier under a
rule that reads one sample per run, and the reason its reds hurt is not that
they are wrong — it is *who they land on*: under the required merge queue, a
price verdict on a name nobody in the composition touched dequeues the whole
composition, every pull request behind it included. Thirteen Fast observations
in those six runs were over `FAST_COMMIT_MS`, and only `i8042_health` was over
it in five of the six; the rest are exactly this shape.

So the audience narrowed instead of the band widening. `.github/workflows/ci.yml`'s
`durations` job now passes `--tier-base <sha>` on a `pull_request` and a
`merge_group`, and `src/durations.rs` renders the *price* verdict only for names
that change registered or re-tiered in `tests/toyos.rs`, or gave a different
`Why` in `src/tiers.rs`'s `RELEGATED`. Every other price verdict prints as a
`::warning::` naming the name, the price and why this run does not enforce it,
and the job exits 0. The nightly's twelve hosted shards pass no base and refuse
them all.

**A nightly red on this name is therefore a finding about this test's variance,
to be fixed at the test.** It is not adjudicated by re-running, not answered by a
`Why::Cost` row — which the return rule refuses the moment a run prices it at or
under 8,000, and its next cheap shard will — and not answered by widening the
band. What is owed is the 3,000 ms of spread: why one boot-and-assert of one
machine costs 4,700 ms on one shard and 9,890 ms on another.

`cargo run -- --known-red xhci_full_speed_device` answers with the row in
`src/redlist.rs`, so an author who meets either refusal does not re-derive the
above.
