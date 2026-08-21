---
status: open
kind: defect
opened: 2026-08-07
---

# Gate A's *thorough* tier reds on an unmodified `main`, and that is the rate `audio-tone-load-fast-tier-intermittent` asked for

`audio-tone-load-fast-tier-intermittent` says "whoever takes it should get the rate first — the thorough
tier (`--audio-gate N`) is the instrument". H3's session got it, and the
instrument reds on the tree it is supposed to certify.

`cargo test --test toyos-build -- --audio-gate 30` on `80fe031` — **main's tip,
no delta at all**, run as H3's A arm before that branch existed:

```
[gate A] FAILED after 15 of 30 iterations (the remaining runs cannot change this):
    pooled dropout rate: 10 of 120 vs recorded 0 of 120 (Fisher p=8.03e-4 <= 1e-3)
```

The ten, by config and iteration: `audio_tone_load smp=1` at 4, 9, 13, 15;
`audio_tone_load smp=8` at 9, 13, 14; `audio_tone smp=8` at 8, 9;
`audio_tone smp=1` at 13. So **`audio_tone` at both widths reds too**, which
`audio-tone-load-fast-tier-intermittent` had only established for
`audio_tone_load`.

**The load correlation is the wrong way round, and that is the finding.** The
1-minute average across the run spanned 7.2 to 19.1 on 14 cores, with one to
five other guests and six other `toyos-build` processes throughout. The clean
early iterations ran at 19.1 and 16.8; the three worst — 13, 14 and 15 — ran at
11.4, 10.6 and 11.9. Every dropout carried a wake latency of 33-117 ms against
5-17 ms on the clean runs, which is the same "soundd was not scheduled"
signature as `audio-tone-load-fast-tier-intermittent` and `one-boot-put-142ms-of-silence-on-the-wire`.

What this changes for anyone reading them: the intermittency is not a property
of one config, and it is **large enough to fail the thorough tier's own pooled
test on a clean tree**. Anything that gates on this tier — the nightly run, and
H3 itself — cannot presently tell its own change from this. H3 therefore
compared its two arms against *each other* rather than against the recorded
sample, and said so.

The recorded sample in `tests/audio-baseline.toml` is 0/120 and was taken in a
session this host no longer resembles. **Re-recording it is not licensed by this
entry** — a baseline widened to accept the defect is the defect made permanent.
What is needed is the cause.

**The B arm was never obtainable, and the reason is `issues/kernel/`'s shootdown deadlock.**
Two attempts on the audio branch stopped at iterations 2 and 4, both on
`audio_tone.smp8`, both with the tier's "instrument broken" verdict — which is
what a guest whose kernel double-panicked looks like from here. Those commits
landed between the two arms and `--land` merged them in, so the arms differ by
more than the change under test and no comparison between them means anything.
What H3 has instead: a full suite green at 289/289 with all four audio configs
clean, and ten standalone runs of the audio family. None of that is a rate.

## 2026-08-21: the CI nightly's red was never this, and its verdicts were never read

**The dev-host finding above stands unchanged.** What follows is about a
different instrument — `gate-a.yml` on a GitHub runner — and it must not be
conflated with it. Nobody re-ran the dev-host arm; nothing here re-runs an audio
verdict away.

`gate-a.yml`'s `gate` step ended in `exit "${PIPESTATUS[0]}"`. The runner logs
the shell it picked for that container on every step — `shell: sh -e {0}` — and
that shell has no `PIPESTATUS` array. Every run answered

```
/__w/_temp/<uuid>.sh: 4: Bad substitution
##[error]Process completed with exit code 2.
```

on the line *after* the gate had printed its verdict. Without `pipefail` the
pipeline's status was `tee`'s 0, so `-e` never fired on the harness's own code;
the step's exit was dash's 2 for a failed expansion, and it was 2 whatever the
audio said. `gate-a.yml` has therefore **never once reported its verdict**:
every run it has ever had is a `failure`, including the ones that passed.

The verdict each shard actually printed, read out of the job logs (artifacts
expire at 30 days; these lines do not):

| run | date | shard 1 | shard 2 |
|---|---|---|---|
| 31386117376 | 08-10 (dispatch, `wt/toyos-ciwave2`) | FAILED `audio_tone.smp8` wake lateness median 6658 → 8496 (z=4.27) | FAILED `audio_tone_load.smp8` wake lateness median 7134 → 9520 (z=6.05) |
| 31771577360 | 08-14 | FAILED `audio_tone.smp8` wake lateness median 6658 → 8673 (z=5.41) | PASS |
| 31862912891 | 08-15 | PASS | PASS |
| 31925451196 | 08-16 | PASS | PASS |
| 31992902784 | 08-17 | FAILED at iteration 25, `audio_tone.smp8` instrument broken — suspend structure | FAILED `audio_tone_load.smp1` wake lateness median 5765 → 17684 (z=4.61) |
| 32097206141 | 08-18 | PASS | PASS |
| 32213928799 | 08-19 | PASS | PASS |
| 32330040225 | 08-20 | PASS | PASS |
| 32445243829 | 08-21 | PASS | PASS |

Thirteen shard-runs PASS, five FAILED, eighteen exits of 2.

**Two consequences, and they point opposite ways.**

The thirteen PASSes mean the standing sentence "the thorough tier reds on
`main`" was being sourced from a red that was not a verdict. It is not evidence
that the dev-host finding above is stale: `gate-a-has-no-runner-baseline` already
establishes that a runner arm compared against the dev host's sample is a
cross-instrument comparison, and since the 2026-08-15 re-record the runner's
numbers are one-sidedly *better* than the recorded ones (08-21 shard 1:
`wake_lat_us recorded 7052/8972/22744 fresh 4002/6034/9942`), which is a
comparison that cannot red. A PASS of that comparison certifies little. The
dev-host question is still open and still needs the dev host.

The five FAILEDs are the harm. Two on 08-10 and one on 08-14 are the
cross-instrument shape `gate-a-has-no-runner-baseline` explains. **The two on
08-17 are not**, and nobody has looked at them: they sit between a PASS night and
a PASS night on the same baseline, and shard 1's is not a statistic at all but
the instrument refusing —
`no 'soundd: suspended' after the last client removal; no 'virtio-sound: stream 0
stopped' after the last client removal — the device is still running with no
clients`. That is filed apart as `gate-a-suspend-structure-verdict-unread`.

The exit code is fixed in `.github/workflows/gate-a.yml` (`set -o pipefail`, the
idiom every other workflow in `.github/` already uses). Nothing about how a
verdict is *reached* changed.
