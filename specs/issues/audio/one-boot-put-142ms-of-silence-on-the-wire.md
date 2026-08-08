---
status: open
kind: finding
opened: 2026-08-03
---

# One boot put 142 ms of silence on the wire, and the host was not quiet

Observed 2026-08-03 while negative-controlling the change above, on tree `602d4e1`
with a harness-only diff — kernel, soundd and the tone client identical to `main`.
`audio_tone_load` smp=1, one boot of four:

```
gaps: total 1 [49p×1]   (142.22 ms of mid-tone silence at 0.813 s)
soundd: wake_lat 46568us (2.01 pipelines)  drains 1  underruns 49  submitted 1203
```

All three instruments agree, which is what makes it one event rather than an
artefact: soundd woke 46.6 ms late — two pipeline depths, so every buffer had
already played out — the pipeline drained once, 49 periods went out with no client
audio behind them, and the capture shows exactly that silence. The recorded sample
for this config is 0/30 dropouts, `underruns` 0 on all 30 runs, and a worst wake of
8250 us; this run is 5.6x that worst wake.

**The host was not quiet.** Another agent's `qemu-system-x86_64` was running in the
primary checkout (observed one second after the run), 1/5/15-minute load averages
6.77/10.19/10.15 on 14 cores. Under the owner's ruling of 2026-08-04 that is not an
excuse and not grounds to re-run it away: the load an audio test puts on this host
is negligible, so a load-coincident stall is a defect of the pipeline until
something shows otherwise. Filed here rather than investigated, per the
one-task-one-agent rule.

Not reproduced in the three other unstaged boots of this config in the same
session (wake 5817, 5280 and 6038 us; `gaps: none`, `underruns` 0 on all three).
The capture was kept by the harness, in its per-pid scratch directory — which is
temporary, so the numbers above are the durable record.

The same session's landing gate carries a smaller instance of the same shape and
no harm at all: `audio_tone` smp=8 at `wake_lat 17050us`, 0.73 pipeline depths and
2.1x the worst wake in that config's recorded 30-run sample, with `gaps: none`,
`underruns` 0 and `drains` 0. Under the verdict above it passes and is printed,
which is the intended reading — one boot, one sample, no audio lost.

The nearest suspect on file is `specs/issues/boot-media/`'s ESP-log flush on the idle path and the
`log_file` flush in `idle_loop` (`specs/issues/kernel/`): unbounded, uninterruptible, and in the one
place a `--smp 1` machine spends the time between audio periods. That is a
hypothesis, not a measurement.
