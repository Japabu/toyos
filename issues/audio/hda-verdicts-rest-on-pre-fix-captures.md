---
status: open
kind: defect
opened: 2026-08-08
task: 88
---

# Two HDA verdicts rest on captures taken through a resampler that is gone

Filed out of the 8.8%-sharp entry when that closed.

Every `hda_tone` capture taken before the sample-base fix went through QEMU's
48000→44100 resampler, which is no longer in the path (dither ratio 3.3% →
24.4%). Two live verdicts were reached on those captures and neither has been
re-taken:

- `#88`'s phase-break verdict (`hda-tone-phase-check`), and
- the mid-tone-silence red.

Neither `EXPECTED_FAILURES` entry was touched when the resampler went. **Both
must be re-judged on a fresh sample rather than carried forward** — a verdict
inherited across a change to the instrument is not a measurement.
