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

The arrays behind every number above are in `0942f02c`'s commit message; the
gate's own logs expire with the workflow artifacts.

## 2026-08-21, four hours later: the number is the *device's* lateness, and the slow mode did not come back

`max_wake_lat_us` now arrives in two halves (`toyos_mixer::WorstWake`), split at
the completion interrupt's own ISR timestamp: `irq` is the device failing to
complete when the grid said it would, `pickup` is soundd failing to run once it
had. They sum to the old number exactly. `late_wakes` counts how many wakes in
the run were a whole period or more late, so the maximum can be read as one
stall or as a thousand.

**296 config-runs on the T14 the same evening, 17:26-19:00 UTC, CI image at the
`route.yml` digest, `--device=/dev/kvm --shard 1/1 --host-slots 0`, no other
container up for any boot** — each block samples `docker ps` every five seconds
and discards and retries itself whole if a CI job appears, which happened three
times and cost three blocks. Two trees, each carrying only the instrument:
`fe41dbae` (`main`, 51 runs per config) and `53101d08` (23 per config) — *the
A/B's own arm B*, the tree that produced the arrays above.

| config | tree | n | wake_lat | irq mean | pickup mean / max | late wakes |
|---|---|---|---|---|---|---|
| `audio_tone.smp1` | main | 51 | 3875-4299 | 4005 | 76 / **113** | 12.9% |
| | `53101d08` | 23 | 3835-4197 | 3969 | 75 / **121** | 12.8% |
| `audio_tone.smp8` | main | 51 | 3977-6176 | 4025 | 139 / **206** | 14.2% |
| | `53101d08` | 23 | 3982-4341 | 3985 | 144 / **188** | 14.2% |
| `audio_tone_load.smp1` | main | 51 | 2509-2986 | 2732 | 10 / **14** | **0.0%** |
| | `53101d08` | 23 | 2542-2981 | 2737 | 10 / **12** | **0.0%** |
| `audio_tone_load.smp8` | main | 51 | 3692-4086 | 3812 | 64 / **142** | 11.1% |
| | `53101d08` | 23 | 3728-4331 | 3881 | 59 / **159** | 11.3% |

Dropouts 0/296, underruns 0/296, drains 0/296, ceiling breaches 0/296. The two
15-iteration blocks that ran the thorough tier both reported **`[gate A] PASS`
— no statistic regressed at alpha=1e-3 per test**, on both trees, against the
same recorded sample the morning's first readable T14 run failed at
`audio_tone.smp1 median 8972 -> 17186 (z=4.36)`. Its fresh medians this evening
were 4075 (`53101d08`) and 4143 (`main`).

Two things fall out of that table and a third out of its absence.

**The statistic is not about the scheduler.** `pickup` never once exceeded
206 µs — 0.009 pipeline depths — on one CPU or on eight, and `irq` is 94-99.6%
of every worst wake. So *which CPU soundd lands on cannot be the mechanism*:
every interrupt lands on the boot CPU (`kernel/src/drivers/pci.rs`'s `MSG_ADDR`)
and a soundd sharing that CPU or not moves a term that is two orders of
magnitude too small to matter. The instrument is not blind to the other half —
the dev host under load 30 reported `pickup 8681us` on `audio_tone_load.smp8`
the same afternoon — the T14 simply never spends it.

**The fast mode is a beat, not an event.** 12-14% of wakes are a whole period
late on the three idle configs, and the worst is ~1.4 periods with `2 empty
wakes` and `batch 2` on essentially every boot. That is soundd's 2.902 ms grid
against QEMU's `timer-period=5000` audio timer: soundd arms, wakes punctually
twice at a device that has produced nothing, and the batch lands ~4 ms after the
grid point. `audio_tone_load.smp1` — the config that is *always* fast — has
**zero** late wakes and `pickup 10 µs`, because a guest with work to do never
lets the beat open.

**And the slow mode did not appear once in 74 boots per config.** Not on `main`
and not on the tree that produced it at 11 of 15 and 9 of 15 four hours earlier.
Under that afternoon's mixing weight, 0 of 74 has probability ~1e-35. So the
mode is **not a per-boot draw from a per-tree distribution**: the distribution
itself moved between two sessions of the same day, on the same host, with
nothing about the tree between them — which also means the A/B's one same-host
row (`audio_tone_load.smp1`, z=4.12) is a difference between afternoons and not
between trees. This evening the two trees are indistinguishable on it: 2509-2986
against 2542-2981.

The host was on AC throughout, `intel_pstate`/`powersave`/`balance_performance`,
`intel_idle` whose deepest state (`C3_ACPI`) costs 1048 µs to leave — a third of
one period, and a fortieth of the 20 ms mode. Nothing on the host was measured
*during* the earlier session, so what moved is not established; the one
difference recorded is that the slow session ran at 1-min load 0.2-1.74 and the
fast one at 1.1-4.4 — overlapping, and with the fast blocks' own quietest
stretches at 1.1-1.6, so load does not separate them either.

## Whoever takes it next

Do not spend the day on soundd. The next sighting of the slow mode is now one
line, and that line already answers three questions: whether it is the device or
soundd (`irq` vs `pickup`), whether it is one stall or a thousand
(`late_wakes`), and whether the guest was executing at all while it happened
(`empty` — a punctual soundd waking repeatedly at a silent device, versus a
single overlong sleep). Gate A's per-boot line also carries the two numbers this
boot drew for its clocks, which are the only per-boot draws that scale every
armed timer for the boot's whole life; on the T14 they are stable to 0.02%
(`tsc 2418-2419MHz`, `lapic 10000742-10002460 ticks/10ms` over 120 boots) and on
the dev host to 0.2%, so a slow-mode boot whose pair sits outside that is a
finding on sight and one whose pair sits inside it removes the whole class.

What is missing is a *host-side* record taken during a slow session, because the
guest-side evidence above points there and cannot go further on its own. The
cheapest one is per-boot `/proc/<qemu>/task/*/schedstat` and the host's
`cpuidle` residencies sampled across a block — and it perturbs the measurement,
so it is worth taking only once a session is producing the mode.
