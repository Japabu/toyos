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
test on a clean tree**. Anything that gates on this tier
(`specs/testing-strategy.md` §5, and H3 itself) cannot presently tell its
own change from this. H3 therefore compared its two arms against *each other*
rather than against the recorded sample, and said so.

The recorded sample in `tests/audio-baseline.toml` is 0/120 and was taken in a
session this host no longer resembles. **Re-recording it is not licensed by this
entry** — a baseline widened to accept the defect is the defect made permanent.
What is needed is the cause.

**The B arm was never obtainable, and the reason is `specs/issues/kernel/`'s shootdown deadlock.**
Two attempts on the audio branch stopped at iterations 2 and 4, both on
`audio_tone.smp8`, both with the tier's "instrument broken" verdict — which is
what a guest whose kernel double-panicked looks like from here. Those commits
landed between the two arms and `--land` merged them in, so the arms differ by
more than the change under test and no comparison between them means anything.
What H3 has instead: a full suite green at 289/289 with all four audio configs
clean, and ten standalone runs of the audio family. None of that is a rate.
