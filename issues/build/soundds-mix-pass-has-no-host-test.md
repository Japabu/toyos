---
status: open
kind: track
opened: 2026-07-28
---

# soundd's mix pass has no host test, and neither negative gate exists

Both soundd defects found on 2026-07-28 were in pure code needing no device, no
syscall and no kernel — and both cost a 120-boot QEMU campaign to find. That is
the case for host-testing the daemons, and it is empirical.

Part of it landed: `toyos-abi` is host-tested, soundd has a `#[cfg(test)]` module
covering quantisation, the deferral floor and shape refusal, and netd/sshd got
their own boot configs. What did not land is the pass itself. Everything from the
mix loop's `loop {` to its stats flush — the free-mask fold, the drain branch,
the refill loop, the pending-removal retain, the stats window — runs only in a
booted guest.

**What to build, and the seam is a value boundary, not a world trait.** One
two-method `PeriodSink` (`buffer(idx) -> &mut [i16]`, `submit(idx)`) plus a
`mix_pass(state, now, records, cmds, sink) -> PassOutcome`. Everything else the
loop needs — the clock, the completion records, the client commands — are
*values* and belong as arguments. Costed at about **+45 productive lines on
1024**, in exchange for ~600 of those lines becoming reachable from a test that
runs in milliseconds. A separate `soundd-core` crate was considered and rejected:
one consumer, one target, one `std`, so the split buys nothing the seam does not,
and costs a lockfile, a re-export layer, ~12 `pub` markers on correctly private
items and `rubato` resolved twice.

The fake device lives in `#[cfg(test)]`: a virtual clock, an inflight queue
mirroring the driver's rejection rules, batched completion masks, and a play
pointer that consumes one buffer per period and **splices silence plus a
starvation count when the queue is empty**. That last rule is the trick — it
turns "soundd woke 35 ms late" from a statistical campaign into an equality. Its
output is a `Vec<i32>` handed to the analyser that already ships, so the host
test and the QEMU gate quantise into the same buckets from one implementation.

**Two negative gates, and the second is the important one.**

- `old_prime_silence_port` — the pre-`91a653c` recovery path behind a feature
  the build never passes, A/B'd on one schedule. Calibration is a recorded
  measurement, not a guess: one deterministic 35 ms mix-thread stall, four
  configs, **278.6 ms of dropout before, 2.9 ms after**. Set the thresholds
  deliberately slack (4× against a measured 96×) so refining the fake device
  cannot silently disarm the gate.
- `old_truncating_quantize_port` — `as i16` where the shipping path rounds. This
  one does not protect the code, it protects the **detector**: a zero-run
  silence detector is viable only against a truncating quantiser, and with a
  correct one the longest run of exact zeros in 4M silent samples measures 47,
  well under the gap floor of 88 — such a detector reports "no dropouts"
  forever. No QEMU boot can catch a disarmed detector, because the boot *uses*
  it. An 11-line pure bug removed the integration gate's detection power once
  already.

Deliberately out of scope, so nobody re-argues it: sshd (~17 pure lines, and its
handler surface cannot be faked without widening a fork); the compositor's `fn
main` decomposition (its two worst defects were found by reading and the right
fix for one is to make it unrepresentable); netd's protocol layer (stock
smoltcp — conformance tests there test upstream); and the resampler's numerics
(libm `sin`/`exp` need not agree bit-for-bit between an arm64 host and the
x86-64 target). Test the code around the resampler, not its output values.

**And the honest ceiling:** none of this touches the wake lateness that causes
the dropouts. It makes soundd's *response* to lateness a controlled variable.
