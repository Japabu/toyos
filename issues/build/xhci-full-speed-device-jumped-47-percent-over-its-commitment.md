---
status: open
kind: defect
opened: 2026-08-21
---

# `xhci_full_speed_device` measured 47% over its committed price

`tests/test-durations` carries `xhci_full_speed_device 6900`. Pull request
#199's `durations` job, run `32513441183`, measured it at **10,166 ms** and
reded the gate:

    xhci_full_speed_device measured 10166 ms in CI, over the 10000 ms line,
    but xhci_full_speed_device remains Fast

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

`cargo run -- --known-red xhci_full_speed_device` answers `NOT ON THE LIST`, so
every author who meets this red re-derives the above.

Two readings and one measurement separates them. Either the shard that measured
10,166 ms was co-scheduled the way `c_capture_ignores_daemon_lines` records
(5,241 ms committed, 12,612 ms and 4,774 ms hours apart on one evening) — in
which case what this test's price measures is contention and the row belongs
beside that one — or something between 2026-08-19 and 2026-08-21 cost it 3,266
ms, in which case the profile's 6,900 ms is stale and the honest answer is to
find the 3,266 ms. Two points cannot say which; the measurement to take first is
its variance across shards of one run, where the co-scheduling reading predicts a
spread and the regression reading predicts a shifted mean.
