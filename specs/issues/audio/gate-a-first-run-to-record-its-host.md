---
status: open
kind: finding
opened: 2026-08-07
---

# The first gate A run to record its host: four of six boots outside the recorded sample, two of them harm

2026-08-07, tree `4a0a07f`, the run that verified the host-conditions
annotation itself (`cargo test -- audio_tone`, filtered). Every line below is
the harness's own, printed beside the counters it qualifies:

```
audio_tone      smp=1  gaps 1 [3p]  underruns 3/1137  drains 8  wake_lat 86862us (3.74 pl)  host: load 49.8/22.7/15.0 qemu 1 toyos-build 4
        confirm        gaps none    underruns 0/1136  drains 0  wake_lat 16968us (0.73 pl)  host: load 49.0/23.4/15.4 qemu 1 toyos-build 4
audio_tone      smp=8  gaps 1 [1p]  underruns 1/1113  drains 0  wake_lat 28118us (1.21 pl)  host: load 48.3/24.1/15.7 qemu 1 toyos-build 4
        confirm        gaps none    underruns 0/1111  drains 0  wake_lat  8434us (0.36 pl)  host: load 46.5/24.1/15.8 qemu 1 toyos-build 3
audio_tone_load smp=1  gaps none    underruns 0/1132  drains 0  wake_lat  7174us (0.31 pl)  host: load 41.2/23.7/15.7 qemu 1 toyos-build 3
audio_tone_load smp=8  gaps none    underruns 0/1126  drains 0  wake_lat 23083us (0.99 pl)  host: load 37.9/23.6/15.8 qemu 1 toyos-build 3
```

**The invocation passed**, and correctly: harm appeared on both `audio_tone`
configs and neither reproduced on its confirming boot, which is precisely what
the two-boot rule is for. What it passed *with* is the finding.

- `audio_tone.smp1` at **86862us — 3.74 pipeline depths, 8.6x that config's
  recorded worst (10090us), and past its 56000us ceiling.** The baseline file
  records `ceiling_runs = 0` across all 120 runs of the 2026-07-31 sample; this
  is the first breach since. It came with `drains 8` — the ceiling exactly —
  three periods of silence on the wire and a 3-period gap in the capture.
- **Two of six boots passed a whole pipeline depth** (3.74 and 1.21), which the
  baseline file states no run of its 120 reached.
- `audio_tone_load.smp8` at 23083us is 2.9x its recorded worst with no harm at
  all — the "bad but real" shape the ceilings exist to admit.

**And now the conditions are on the record rather than reconstructed.** 1-minute
load 37.9-49.8 on 14 cores with three to four other `toyos-build` processes and
no other guest, against the 4.2-6.1 the 2026-07-29 ceiling derivation recorded
per run — six to twelve times it. Under the owner's ruling of 2026-08-04 that is
**not** an excuse and not grounds to re-run it away: it is a defect of the
pipeline until something shows otherwise, and it is the same shape as the two otherentries beside it. What is new is only that the next investigation starts from a
measured host state instead of a guess.

Whoever takes it: the thorough tier is the instrument for the rate, and it now
prints `host conditions over N runs` so its own arm's conditions can be stated.
The recorded arm's cannot — see `tests/audio-baseline.toml`.
