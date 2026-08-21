---
status: open
kind: finding
opened: 2026-08-10
---

# Gate A's thorough tier on a runner compares against the dev host's sample, and needs its own

`tests/audio-baseline.toml`'s recorded sample was taken on the dev host under
cross-arch TCG. The thorough tier compares a fresh sample against *that*, so
`gate-a.yml` on any KVM runner is comparing two instruments and calling the
difference a regression. `route.yml` now sends this gate to the T14 by default,
so that is what every nightly does.

## Settled 2026-08-21: it is the instrument, and the control says so

The earlier version of this file inferred the cross-instrument gap from level
differences. It is now measured against a same-session control, which is what
the audio law requires before a harm verdict may be set aside.

**The experiment.** Four interleaved 15-iteration blocks of
`cargo test --test toyos-build -- --audio-gate 15 --shard 1/1 --host-slots 0`
on the T14, in the CI image at the digest `route.yml` names, `--device=/dev/kvm`,
QEMU 11.1.0 — the CI invocation, with private checkouts and a private cache root
so the runner's own state was untouched. Order A,B,A,B; 30 iterations per arm.

* arm A = `960b96e3`, **the tree the recorded sample was taken on**
* arm B = `53101d08`, `main`

Every block was gated on the machine being idle first and carried a witness
sampled every 10 s: no CI job container was present for any of the 240 boots,
and the 1-minute load stayed in 0.2-1.74.

**The verdict, by the gate's own Mann-Whitney at its own alpha (1e-3, z>3.0902):**

| config | recorded | T14 arm A | T14 arm B | A vs B (n=30) |
|---|---|---|---|---|
| `audio_tone.smp1` | 8972 | 20314 | 19994 | z=1.49 — **no difference** |
| `audio_tone.smp8` | 9249 | 14069 | 4088 | z=5.37 — B faster |
| `audio_tone_load.smp1` | 5765 | 2764 | 4352 | z=4.12 — B slower |
| `audio_tone_load.smp8` | 6097 | 12676 | 3904 | z=5.48 — B faster |

Medians of `max_wake_lat_us`, microseconds.

**The negative control is the whole finding: arm A fails this baseline on the
T14 and arm B does not.** Both arm-A blocks red `audio_tone.smp1` against the
recorded sample — `median 8972 -> 20438 (z=5.03)` and `8972 -> 20144 (z=4.67)`
— and both arm-B blocks print `PASS`. The tree the sample was recorded on reds
its own sample on this host, *harder* than `main` does. A level difference that
reds the recording tree is not a regression in anything.

So gate A run `32479089989`'s verdict —
`audio_tone.smp1 wake lateness: median 8972 -> 17186 (z=4.36)`, the first
readable exit this workflow ever produced — is adjudicated: **the instrument,
not the tree.** No re-run was used to reach that; a same-session interleaved
control was.

**Harm was null on both arms**, which is why this is a comparability finding and
not a defect: dropouts 0/120 and 0/120, underruns 0 in all 240 config-runs,
drains all-zero but for a handful of single events, ceiling breaches 1/120 on
arm A (a 334604 us single wake on `audio_tone.smp8`, no dropout behind it) and
0/120 on arm B.

## What is still owed, and it is more than pasting numbers

**Nothing here licenses replacing the recorded sample with a T14 one.** The dev
host still runs the fast tier against it, and a KVM sample would be as wrong
there as the TCG sample is on the runner. What is needed is a baseline *per
host*, and two things block writing one:

1. **The file has no host dimension and the loader has no way to pick one.**
   `AudioBaseline` is `BTreeMap<test, BTreeMap<smpN, entry>>` with
   `deny_unknown_fields`, and `config_baseline` (`tests/toyos.rs`) selects on
   `(name, smp)` and nothing else. A `[runner]` sample is therefore a schema
   change plus a selection keyed on whether the accelerator is in use — a change
   to how a high-risk gate decides, not an edit to a table.
2. **The T14 distribution is bimodal per boot, and the mode's probability moves
   with the tree.** Recording 30 runs of it would freeze a mixture whose mixing
   weight is the thing that varies. Measured and written up in
   `issues/audio/t14-wake-lateness-is-bimodal-per-boot.md`, which is what has to
   be understood before any T14 number is worth recording.

The 2026-08-10 measurement this file opened with — run `31386117376`, tree
`99e47d9`, two GitHub-hosted runners of different vendors — remains the reason a
hosted sample would be a sample over two unnamed CPUs. Its `wakes` observation
still holds and the T14 reproduces it: 1185-1432 fresh against 846-905 recorded,
a KVM guest waking about 1.6x as often as the same guest under cross-arch TCG.
That artifact expired on 2026-09-16; the T14 arrays above are in this branch's
commit message.
