---
status: open
kind: defect
opened: 2026-08-21
---

# Gate A refused its own instrument on 2026-08-17 and nobody read the verdict

`gate-a.yml`'s 2026-08-17 nightly (run `31992902784`) stopped shard 1 at
iteration 25 of 30 with the thorough tier's hardest verdict — not a statistic,
the instrument declining to measure:

```
[gate A] FAILED on iteration 25: audio_tone.smp8 instrument broken: suspend
structure: no `soundd: suspended` after the last client removal; suspend
structure: no `virtio-sound: stream 0 stopped` after the last client removal —
the device is still running with no clients
```

Shard 2 of the same run failed on a statistic:

```
[gate A] FAILED — 1 statistic(s) regressed:
    audio_tone_load.smp1 wake lateness: median 5765 -> 17684 (Mann-Whitney z=4.61 > 3.09)
```

**Neither was ever adjudicated, because the exit code could not tell anyone they
had happened.** That workflow's step ended in `exit "${PIPESTATUS[0]}"` under a
shell with no such array, so it reported `failure` on every run whatever the
audio said; 08-17's two FAILEDs and the PASSes on 08-16 and 08-18 arrived as the
same red. The mechanism and the full run-by-run table are in
`issues/audio/thorough-tier-reds-on-unmodified-main.md`; the exit code is fixed,
these two verdicts are not.

**Why this one is not the cross-instrument shape.** Three of the five FAILEDs
that workflow ever printed are the runner-vs-dev-host wake-lateness level
difference `gate-a-has-no-runner-baseline` explains, and they are all from before
the 2026-08-15 re-record. These two are after it, and they sit between a
two-shard PASS night (08-16) and a two-shard PASS night (08-18) on the same
recorded sample. A level difference does not come and go for one night.

**Shard 1's is the one to take first.** `soundd: suspended` and
`virtio-sound: stream 0 stopped` are the two lines that say the idle path
released the device; their absence says a boot left the device running with no
clients, which is the subject of `stop-the-device-voice-keep-the-wake` and
`idle-suspend-reds-on-a-loaded-host-and-on-main` from the other side. One
occurrence in 25 iterations is a rate nobody has, and the tier is the instrument
that produces one.

**The evidence expires.** `/tmp/gate-a.log` is uploaded per shard with
`retention-days: 30`, so run `31992902784`'s artifacts go on 2026-09-16; the job
logs outlive them. Everything quoted above is already here for that reason.

Whoever takes it: `gh workflow run gate-a.yml -f iterations=30` now reports its
own verdict, so a re-dispatch is a readable experiment for the first time.
