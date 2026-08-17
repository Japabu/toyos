---
status: open
kind: defect
opened: 2026-08-17
---

# The check-build guest stopped answering on KVM, independently, twice

`sched_check_build` boots the `sched-check` kernel and asserts invariant P
holds its 200 µs per-pass budget.
`specs/issues/kernel/invariant-p-cannot-hold-under-cross-arch-tcg.md` already
covers the dev host's own red — cross-arch TCG cannot meet that budget, and
`src/redlist.rs` scopes that row to `Instrument::DevHostAlone` so it cannot be
read as a CI finding. This is the other instrument: two KVM shards have now
STALLed the same name, on two different pull requests a day apart, and both
times the guest that stopped answering came back `ALONE: GREEN`.

## Two sightings, not one

**2026-08-15, run 31890991692, guest 8** (`wt/toyos-logd56`). STALL, then
`ALONE sched_check_build: GREEN`.

**2026-08-16, run 31946183485 (pull request #95, `wt/toyos-harness2`), job
95162423932, guest (8)**:

```
FAIL sched_check_build: the check-build guest stopped answering, which is
what a scheduler assert firing looks like from here: STALLED: 382s of guard
expired, and the guest had said nothing for the last 383s of it — the
ceiling caught a machine that had stopped, which is not an answer to what
this test asked
```

then, isolated:

```
PASS  sched_check_build  (6s)
ALONE sched_check_build: GREEN, and it was alone both times — nothing the
harness controls differed, so it failed once and passed once. That is a
rate and not a classification.
```

`durations` (job 95163708113) reds too, but purely as the STALL's own
arithmetic, not a second defect: `sched_check_build measured 387502 ms in
CI, over the 10000 ms line, but sched_check_build remains Fast` — the
number is the guard's 388s, not a slow test.

## What is not yet known

Whether the guest genuinely stopped — a real hang, in a build that has never
before failed to answer *at all* inside its guard — or whether 388s is too
tight for what a shared GitHub Actions runner can owe a guest during a bad
minute. Neither sighting left anything behind that says what the guest was
doing for those ~385 s: STALL is exactly the harness declining to answer that
question, on purpose, because a guard that already expired has nothing left
to say about the tree. Both sightings landed on the same shard index, guest
8 of 12 — one chance in twelve if shard assignment is uniform and has
nothing to do with the stall, and two samples cannot tell that apart from a
pattern.

`src/redlist.rs`'s `sched_check_build` row records both sightings as
qualifying prose on its `Instrument::DevHostAlone` row rather than a row of
their own — a STALL is a duration and not a verdict, so it cannot become a
`Finding` — which is why the pattern needed a file of its own to carry an
owner.
