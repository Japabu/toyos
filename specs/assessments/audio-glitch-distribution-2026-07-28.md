# Audio glitch gate (Gate A) — measured distribution and diagnosis

**Date:** 2026-07-28 · **Tree:** `35913b0` (clean, unmodified) · **Host:** Apple M4 Pro
(10P+4E), macOS, QEMU 11.0.2 under cross-arch TCG (no KVM/HVF).

This document records what `tests/audio-baseline.toml` actually gates on, measured
rather than assumed. Nothing in the tree was changed to produce it; the baseline
file is untouched.

## 1. Method

`cargo test -- audio --nocapture`, run **strictly serially**, one QEMU at a time.
Each invocation boots four dedicated guests: `audio_tone` and `audio_tone_load`,
each at smp=1 and smp=8. Recorded per run: the harness gap histogram, the
pass/fail verdict, soundd's own `wakes/completions/submitted/underruns/reprimes/
max_wake_lat_us` line, and the host 1-minute load average at iteration start.
Every failing capture was preserved and re-analysed independently of the harness
(`zero runs` at all lengths, positions relative to tone onset, total active
signal, click detection).

Two batches:

- **Quiet host** — 30 iterations (120 config-runs), desktop-idle, 1-min load 4.0–8.6.
- **Loaded host** — 12 iterations (48 config-runs) with 10 host spinners, load ~8.5–14,
  to quantify the host-contention confound.

## 2. Distribution — quiet host, 30 runs per config

| Config | FAIL | gaps per run |
|---|---|---|
| `audio_tone` smp=1 | 2/30 (7%) | 0 gaps in 28, 1 gap in 2 (one 1p, one 8p) |
| `audio_tone` smp=8 | 0/30 (0%) | 0 gaps in 30 |
| `audio_tone_load` smp=1 | 3/30 (10%) | 0 gaps in 27, 1 gap in 1 (8p), 2 gaps in 2 (16p+80p; 24p+40p) |
| `audio_tone_load` smp=8 | 2/30 (7%) | 0 gaps in 28, 1 gap in 2 (one 1p, one 16p) |

**7 of 30 full audio-suite invocations (23%) are red on a quiet host.**
Gap sizes observed, in device periods (2.902 ms): `1×2, 8×2, 16×2, 24×1, 40×1, 80×1`.

soundd counters, per 2 s stats window (min / median / max):

| Config | max_wake_lat (ms) | reprimes | client underruns | wakes |
|---|---|---|---|---|
| `audio_tone` smp=1 | 6 / 16 / 40 | 2 / 4 / 5 | 0 / 5 / 13 | 490 / 524 / 575 |
| `audio_tone` smp=8 | 6 / 10 / 14 | 1 / 2 / 2 | 1 / 4 / 8 | 572 / 589 / 611 |
| `audio_tone_load` smp=1 | 12 / **37** / **70** | 5 / **10** / **24** | 0 / 15 / 28 | **213 / 253 / 269** |
| `audio_tone_load` smp=8 | 7 / 22 / 44 | 1 / 3 / 4 | 2 / 4 / 7 | 544 / 589 / 615 |

The pipeline holds 8 × 2.902 ms = **23.2 ms**. Median wake lateness on
`audio_tone_load` smp=1 is 37 ms — already past the pipeline depth — in *passing*
runs. soundd completes 213 mix cycles where it should complete ~590: its effective
cycle time under single-CPU load is ~8 ms against a 2.9 ms period.

## 3. Real underrun, not a harness artifact

> **[2026-08-01] The underruns this section diagnoses are gone, and the reasoning is
> still correct.** `tests/audio-baseline.toml`'s current sample records `underruns = 0`
> on all 120 config-runs across the four configs. Two instrument faults (`824dd7d`,
> `7095046`) and one real defect (`069d158`) were closed after this was written, and
> `aeeaa01` removed the timer-vs-completion race behind the second timing mode.
>
> The four lines of evidence below are why the underruns were believed real rather than
> a harness artifact — a question that will be asked again the next time the gate goes
> red, which is why this is a marker and not a rewrite. **Do not cite the numbers here
> as current.** `tests/audio-baseline.toml` is the live figure.

Four independent lines of evidence, all pointing the same way.

**3.1 The capture is a true real-time timeline.** Every wav is ~6.7 s: leading
silence from device start to first client audio, exactly 3.0 s of tone, then
trailing silence to shutdown. Silence is recorded, not skipped.

**3.2 The tone is never damaged — silence is inserted.** Across all 16 preserved
failure captures the tone content is invariant to the millisecond: `active(>500)
= 2.940 s`, `peak = 15999`, `clicks = 0`, and exactly 59 intrinsic 1-frame zeros
(the 440 Hz sine's exact zero crossing every 50 ms). The signal *span* grows by
precisely the summed gap length. No samples are lost; the device played silence
while client audio waited. That is the definition of an underrun.

**3.3 Every gap is period-quantised, and the large ones are quantised to the
pipeline.** All gaps are exact multiples of 128 frames. On the quiet host, every
gap ≥ 8 periods is an exact multiple of **8** periods — 8, 16, 24, 40, 80 — and
nothing else. 8 periods is `TX_INFLIGHT_MAX`, the whole DMA pipeline. There is no
9p, 10p or 12p gap in any capture.

**3.4 soundd's own accounting corroborates, independently of the wav.** In the
worst quiet-host failure (iter 006, `audio_tone_load` smp=1, gaps `80p + 16p` =
96 periods = 12 pipeline-fulls) soundd reported `underruns=0 reprimes=24
max_wake_lat_us=70167`. `underruns=0` means the client always had data; the
silence came from soundd's own recovery path. 12 of the 24 reprimes landed inside
the tone; 96 = 12 × 8 exactly.

The harness's **positives are trustworthy**. Its **negatives are not** — it is
blind to dropouts outside the tone, and it samples only ~1000 periods per run.

The gap detector cannot false-positive on this signal: the tone's only intrinsic
zeros are single frames, well under the 2 ms floor, and soundd's TPDF dither
truncates to exactly 0 for a silent mix (`(0.0*32767 + d) as i16`, |d| < 1), so
mixer silence really is digital zero.

## 4. Mechanism

Two distinct failure modes, with distinct signatures:

**(A) Pipeline drain + reprime → gaps that are multiples of 8 periods.**
When soundd is late by more than the 23.2 ms pipeline, all 8 buffers are free.
`soundd/src/main.rs` §5.9 then does:

```rust
if free_mask.count_ones() as usize == num_buffers {
    stat_reprimes += 1;
    prime_silence(&mut free_mask);   // zeroes and submits ALL 8 buffers, free_mask = 0
    dll.reset();
}
while free_mask != 0 { /* mix client audio */ }   // skipped entirely
```

The recovery **commits a full pipeline of silence and skips mixing for that
cycle**. A 24 ms stall and a 40 ms stall cost the same 23.2 ms of audible silence,
and consecutive stalls concatenate: 10 in a row produced the 232 ms gap. The
recovery cost is a step function, which is exactly why the gap histogram is
quantised to multiples of 8.

**(B) Client slot miss → gaps of 1–7 periods.** `mix_client` finds no slot,
returns false, soundd mixes silence for that client and counts `underruns`.

**Where the lateness comes from.** On the quiet host, mode (A) dominates (7 of 9
gaps) and is concentrated on **smp=1 under load**: the same iteration, seconds
apart, shows `audio_tone` smp=1 at 6–16 ms lateness and `audio_tone_load` smp=1 at
37–70 ms. smp=8 — which demands *more* host CPU — is consistently *better*
(22 ms median). A host-side cause cannot produce a penalty that appears only when
the *guest* has two normal-priority spinners while the guest with more vCPU
threads does better. The dominant cause is in-guest: **ToyOS does not keep the RT
mix thread inside its 23.2 ms budget on a single CPU under normal-priority load.**

The kernel machinery for this reads correct on inspection — strict RT-band-first
`pick_next`, `set_need_resched` from the virtio-sound MSI-X ISR, a deferred-preempt
epilogue on the Ring 3 return path, and a one-shot timer armed at
`QUANTUM_NS.min(next_deadline - now)`. Nothing in the failing window correlates
with kernel activity in the serial log (no spawn, no fault, no logging near the
232 ms stall). Pinning the exact stall site needs **Layer 2 event tracing**, which
is not built. This is as far as the evidence goes without it.

Note also that `audio_tone` smp=1 on a fully idle guest still shows 2–5 reprimes
and 6–40 ms wake lateness per window. Even the unloaded single-CPU case does not
hold the pipeline.

## 5. Host load is a real confound — for the earlier measurements

| Batch | suites red | config-runs failed | gap sizes |
|---|---|---|---|
| quiet (30 suites) | 7/30 = **23%** | 7/120 = 5.8% | mostly multiples of 8 (7 of 9) |
| loaded (12 suites) | 9/12 = **75%** | 11/48 = 22.9% | mostly sub-8 (15 of 17) |

Host load roughly **quadruples** the red rate and **changes the failure mode**:
under host contention the client thread misses its refill and produces small 1–7
period gaps, whereas on a quiet host the failures are soundd pipeline drains. One
loaded run also hit the harness's 30 s per-test timeout.

Within the quiet batch there is **no** correlation between the 1-min load average
at iteration start and the outcome (failing runs mean 5.09, clean runs mean 5.27) —
load average at that granularity does not predict a run. Concurrent QEMU/build
activity is a genuine confound for the earlier "2 of 19" and "2 of 4" samples, but
it does not explain the residual: the quiet-host rate is 23% per suite invocation.

## 6. What an honest baseline would say (recommendation only — owner decides)

`tests/audio-baseline.toml` was **not modified**. The options, with their audible
consequence stated plainly:

1. **Keep all-clean (strict zero-gap).** Honest about the quality target; the
   file correctly says "a clean run has no dropouts". Consequence: the audio
   suite is red ~23% of the time on a quiet host, and Gate A cannot be described
   as always-green. *Recommended.*
2. **Record the measured worst case** (e.g. `audio_tone_load.smp1 = {8=1, 80=1}`).
   Consequence: the gate would certify a build that drops **232 ms** of audio
   mid-tone as "no regression". Since `check_gap_regression` only compares total
   count and longest class, this makes the gate accept nearly anything.
   *Not recommended — this is a decision to accept clearly audible dropouts.*
3. **Change the statistic, keep the bar.** Gate on N repetitions per config
   (e.g. "≥28 of 30 runs clean, no gap over 1 period"), which keeps zero-gap as
   the quality target while making the gate reproducible. Costs N× runtime.
4. **Fix the defect**, then 1 becomes achievable per-run.

A cheap improvement orthogonal to all four: **gate on soundd's own counters**
instead of only on the wav. `reprimes` and `max_wake_lat_us` are already measured,
are non-zero on essentially every run, and are direct evidence of the stall — a far
more sensitive instrument than a 3 s wav that samples ~1000 periods. This needs
soundd to count reprimes *while streaming* separately from idle-phase reprimes
(today's counter mixes them).

## 7. Can Gate A certify scheduler stages 6–9?

**No, not as currently constituted.** One run per config per invocation, against a
per-config failure probability of 0–10%, gives errors in both directions:

- **False red:** a clean build shows red in 23% of suite invocations.
- **False green:** a regression would have to be large to be visible. Going from
  10% to 20% failure on one config is undetectable in a single run.

What would have to change, in order of value:

1. **Repeat.** Gate on ≥20–30 runs per config and compare rates, not a single
   Bernoulli trial. Everything else is secondary to this.
2. **Measure the mechanism, not its tail.** Assert on soundd's in-guest counters
   (streaming reprimes, `max_wake_lat_us` vs pipeline depth). These are non-zero
   on nearly every run today, so they have real statistical power; the wav
   histogram is a rare-event detector.
3. **Control the host.** Host load quadruples the rate and changes the failure
   mode. Gate runs must be serial and on a quiet machine, and should record the
   host load with the result.
4. **Fix mode (A)** so the recovery path costs proportionally rather than a full
   pipeline of silence — and fix the underlying single-CPU RT starvation. Until
   then Gate A is measuring a subsystem that is already out of spec, and "no
   regression" is a claim about the tail of a broken distribution.

Raw logs, preserved failure captures, and the analysis scripts for this run live
outside the repo (session scratchpad); the numbers above are reproducible with
`cargo test -- audio --nocapture` run serially.

## 8. Addendum (same day, after the §5.9 and §5.4 fixes): what the gate became

§7 said no, and listed four things to change. Three of them are done; the
numbers moved, and the conclusion moved with them.

**The distribution changed.** After the proportional-recovery fix (mode A) and
the quantizer fix, re-measured over the same 30 serial invocations on the same
quiet host: dropout-producing runs are `audio_tone` smp=1 **1/30**, smp=8
**0/28**, `audio_tone_load` smp=1 **2/30**, smp=8 **1/29** — pooled **4 of 117**
(3.4%), against 7/120 before, with the ×8 quantisation gone. Suite-level: 3 of
30 invocations red on dropouts, not 7.

**Item 3 (control the host) is procedure, not code**, and it stands: serial
runs, one QEMU, quiet machine.

**Item 4 (fix the defect) is not done and is still scoped out.** Every config's
worst wake still exceeds one pipeline depth; `audio_tone_load` smp=1 peaks at
**93.0 ms = 4.0 pipeline depths**. 9 of 120 runs wake later than the pipeline
they are feeding. The gate records that rate rather than hiding it.

**Items 1 and 2 are done, and they are what makes the answer yes.** Gate A now
has two tiers (`tests/audio-baseline.toml` documents both):

- The **fast tier** is what `cargo test` runs. It keeps the strict zero-gap bar
  but confirms a dropout with one re-boot before failing, which takes the
  clean-tree red rate from 12.8% per invocation to 0.67% while still firing 25%
  of the time against a config that regressed to a 50% dropout rate.
- The **thorough tier** (`cargo test --test toyos-build -- --audio-gate 30`, ~17 min) is what a
  stage transition gates on. It compares a fresh 30-run sample against the
  recorded 30-run sample — Mann-Whitney for soundd's counters, Fisher exact for
  the yes/no outcomes — and detects a 25% shift in wake lateness 99.9% of the
  time, a 5% drop in soundd's wake count 99.9%, a 50% rise in periods of silence
  100%, and a 10× rise in the dropout rate 100%. False-red on a clean tree:
  0.25%, over 2000 invocations simulated from the recorded distributions.

**So: yes for stages 6–9, with one stated limit.** The thorough tier can
certify that the audio timing distribution has not shifted, which is what a
scheduler change would move. It cannot certify that the *dropout rate* has not
doubled — separating 3% from 7% at this confidence needs ~600 runs per config
(five hours per config), and that is a property of a 3% Bernoulli event, not of
the harness. Any claim of the form "stage N did not increase dropouts by less
than 2×" is outside what this instrument can support, and §7's warning applies
to it unchanged.

Two harness defects found while measuring this, both fixed, both of which had
been presenting as guest faults:

1. `run_test` matched `===TEST_END ` as a **line prefix**. The virtio-console is
   shared and not line-atomic, so soundd mid-`println!` pushes the marker into
   the middle of its line; the harness then misses it and times out after 30 s.
   1 of 120 audio boots. It looked exactly like a guest hang.
2. The soundd stats parser could not read a line another writer had split. 2 of
   120 boots. Both now splice across the interruption.

Neither was a kernel problem, and both were counted against the kernel before
they were found.
