---
status: open
kind: finding
opened: 2026-08-21
---

# On the T14 soundd's worst wake is bimodal per boot — ~4 ms or ~20 ms — and which mode a config lands in moves with the tree

Measured on the self-hosted T14 (Intel i5-1135G7, 4c/8t, KVM, QEMU 11.1.0, CI
image) during the A/B that settled
`issues/audio/gate-a-has-no-runner-baseline.md`: four interleaved 15-iteration
gate A blocks, A,B,A,B, arm A `960b96e3`, arm B `53101d08`, idle machine, no CI
job container for any of the 240 boots, 1-min load 0.2-1.74.

`max_wake_lat_us` does not vary continuously on this host. It takes one of two
values per boot:

* a **fast mode** at 2544-4352 us (0.11-0.19 pipeline depths), and
* a **slow mode** at roughly 10000-25000 us (0.43-1.08 pl),

with nothing much in between, and the mode is drawn per boot rather than
drifting through a block. `audio_tone.smp1`, arm B, in run order — the fast runs
are scattered, not clustered at either end:

```
b1  21389 18528  3945  4183 19994 20483 21544 33934 21591 21852 21139 22114  3871 16861 21481
b2   4010 21562  4085  4028  4028  4212 14811 21692 17017 19528 22652 20878 18474 24184 11543
```

Arm A, same config, has essentially no fast runs at all (one of 30, at 9880),
and arm B has eight of 30 — yet the two arms are **indistinguishable** on this
config overall (medians 20314 and 19994, z=1.49), because the slow mode
dominates both.

Where the arms *do* differ, they differ by which mode dominates:

| config | arm A, n=30 | arm B, n=30 | A vs B |
|---|---|---|---|
| `audio_tone.smp1` | 20314, one fast run | 19994, eight fast runs | z=1.49, same |
| `audio_tone.smp8` | 14069, mixed | 4088, **all 30 fast** (3939-4240) | z=5.37, B faster |
| `audio_tone_load.smp1` | 2764, **all 30 fast** (2544-3259) | 4352, mixed | z=4.12, B slower |
| `audio_tone_load.smp8` | 12676, mixed | 3904, **all 30 fast** (3643-4241) | z=5.48, B faster |

## Why this matters more than the direction of any one row

**The slow mode has no margin.** 20 ms is 0.86 of the 23219 us pipeline depth —
the point at which every buffer has drained and the device has run out of audio.
The recorded dev-host sample reaches 0.98 pl once in 120 runs and sits at 0.39 pl
in the median; on the T14 the *median* `audio_tone.smp1` boot, on both arms, is
where the dev host's worst run was. Nothing was audible in any of the 240 boots
— dropouts 0/120 and 0/120, underruns 0 in all 240 config-runs — but the
distance to harm on a slow-mode boot is one scheduling accident.

**And a bimodal statistic is a bad thing to baseline.** The thorough tier's
Mann-Whitney is comparing mixtures, so its verdict tracks the mixing weight
rather than either mode. That is why no T14 sample should be recorded into
`tests/audio-baseline.toml` until the mode is understood: a re-record would
freeze one afternoon's mixing weight and red on any tree that moved it.

## The one row that is a real same-host difference, and why it is not called a regression here

`audio_tone_load.smp1` is worse on `main` than on `960b96e3` at z=4.12 pooled —
and it is the same config the never-read 2026-08-17 hosted nightly failed on
(`median 5765 -> 17684, z=4.61`, quoted in
`issues/audio/gate-a-suspend-structure-verdict-unread.md`). It is not called a
bisected regression because the block structure says it is not stable: the
per-block figures are z=3.57 (a1 vs b1) and z=2.09 (a2 vs b2), and arm B's own
two blocks differ by z=2.51 against arm A's z=0.35. Arm B is *unstable* on this
config; arm A is not. That is a change in the mixing weight, not a level shift,
and bisecting a mixing weight at n=15 per point would measure noise.

## Whoever takes it

The mechanism is what is missing, and the instrument now exists to find it: the
T14 reproduces both modes on demand in ten minutes per 15 iterations, and
`audio_tone.smp8` on arm B is 30 of 30 in the fast mode — a config that is
*always* fast is the control a mechanism has to explain. Start from what differs
between a fast boot and a slow one in soundd's own counters: `wakes` moves only
1354-1371 across both modes on `audio_tone.smp1`, so the pipeline is being
retired the same number of times either way, and the difference is *when* rather
than *how often*.

The arrays behind every number here are in this branch's commit message; the
gate's own logs expire with the workflow artifacts.
