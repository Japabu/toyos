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
