# Gate A history: four instrument defects and one real dropout

How the audio glitch gate got from "0 gaps, verified" (which was too strong) to a
recorded, honest baseline, and how the post-cutover dropout regression was
bisected, mis-attributed twice, and finally fixed in soundd. Split out of
CLAUDE.md 2026-07-30; everything here is **closed**. The live bar lives in
`tests/audio-baseline.toml`, the pre-fix distribution in
`specs/audio-glitch-distribution-2026-07-28.md`.

All of it happened on one laptop host. The single most reusable lesson: these
counters drift between batches on one host with no code change — 7a measured at
the start of a session (`wakes` ~1030, `drains` 4/4/4/3/3) and 7a measured 40
minutes later from the identical commit (`wakes` ~884, `drains` 5/6/4/4/3) do not
overlap. **Only same-session A/B numbers mean anything.**

## The baseline was aspirational, then honest

`tests/audio-baseline.toml` originally recorded all-clean. Measured 2026-07-28
during Stage 5 it was not: 2 failures in 19 runs, with soundd's own underrun
count spanning 0–17 across runs that all "passed". The pre-fix history is not
undone by that — the baseline before the audio work was 1,834 gaps — but "0 gaps,
verified" was a claim the instrument could not support.

The dominant contributor was soundd's re-prime path
(`specs/audio-glitch-distribution-2026-07-28.md` mode A). After the fix, 30
serial suite invocations per side, 120 config-runs each: 7/30 → 1/30 suites red,
12 → 1 gap events, 464 ms → 29 ms of total mid-tone dropout, and the ×8
quantisation gone (8 events / 418 ms of multiples-of-8 → zero).

Re-recorded fresh over 30 invocations / 120 config-runs: `audio_tone` smp=1 1/30,
smp=8 0/30; `audio_tone_load` smp=1 2/30, smp=8 1/30. Pooled 3.4%. The `gaps`
histograms stayed strictly zero — the bar was not loosened.

**The statistic was still wrong.** One run per config per invocation is a
Bernoulli trial against a 3.4% rate: it reds a clean tree on 12.8% of invocations
and is blind to a doubling. Gate A became two-tiered (see CLAUDE.md's Build &
test); the fast tier confirms a dropout with a second boot before failing, and
stage transitions gate on `--audio-gate 30`.

The file was re-recorded for real at `dc732e5`, once all four instrument defects
below were fixed and the dropout was gone.

## The four instrument defects

Each of these made a counter measure something other than what its name said, and
each was mistaken for a scheduler regression first.

**1. `underruns` counted connect-time pre-roll** (fixed `f25fa87`). It counted
every silent period from the moment a client *connected*, but a client sends
`MSG_STREAM_OPEN` before it has any audio. Proof, arithmetically, from the
pre-fix runs: `submitted − 1034 tone periods − underruns` = 187–300 ms on all
four configs, i.e. exactly the tone client's own post-tone `sleep(200ms)` of
delivered zeros, leaving `underruns` == the pre-roll to the period. soundd now
counts a silent period only while some stream `is_streaming()`. Same-session A/B,
3 invocations per side: `audio_tone_load` smp=1 121/106/90 → 43/43/37.

**2. `max_wake_lat_us` measured against a prediction nobody armed** (fixed
`824dd7d`). `lateness = clock_nanos() - dll.t_estimated`, and the first
completion after a client connects was measured against a `t_estimated`
established during the *idle* phase — where soundd runs with `timeout = u64::MAX`
and arms no timer at all. Traced in the guest: every one of seven breaches was
its window's first sample, against a `t_estimated` 56–150 ms old. The counter now
measures against the prediction the wait was **armed on** (`armed_on`, captured
where the timeout is computed). A late wake stays fully visible at any magnitude;
only a wait with no armed deadline is silent, and there is nothing to be late for
in one. A/B: parent 3 of 22 config-runs over 55 ms (105479 / 157917 / 71076),
fixed 0 of 27, max 23720 us = 1.02 pipelines, median ~7.8 ms.

This is where the recorded 153-second wake lateness came from
(`iter014: ... wake_lat 153766519us`, inside a 3-second test).

**3. `drains` counted early wakes as dry pipelines** (fixed `7095046`). It
counted every cycle that found the whole DMA pipeline free, but for ~50 ms after
a stream starts QEMU hands the entire 23.2 ms pipeline back every 0.7–6.6 ms — up
to 34x real time — so soundd was woken *early* and still saw an empty free list.
A drain now counts only when soundd named a wake time *and* when at least
`(num_buffers - 1)` periods of wall clock have passed since the last refill, a
bound no device playing at its own rate can beat. Traced in-guest over 49
full-free events: every streaming-phase one was 0.7–6.6 ms after the refill
against a 20.3 ms floor. Per-config `drains` 5/4/9–12/6–10 → 0 across 108
post-fix config-runs.

**4. The gate ranked physically impossible values instead of rejecting them**
(fixed `0a8b480`, `audio::check_physical`). The thorough tier applied no per-run
ceiling, only distributional comparison, and Mann-Whitney is rank-based — so the
153-second reading did not fail anything, and worse, the gate's toml-ready output
carried `max_wake_lat_us = [..., 24593, 153766519]`, which pasted in would set
the ceiling to 2× that and permanently disable the check.

The bound is the wall-clock life of the QEMU process, timed by the harness:
soundd's whole life is inside it, so no duration it reports can exceed it, and no
count of device periods can exceed the periods that fit in it
(`underruns <= submitted` rides along as a definitional check). Nothing is
recorded in the toml, so there is no number to tune when a run goes red. Verified
by injecting the recorded 153766519us: the fast tier fails it as a *broken
instrument*, the thorough tier aborts on iteration 1 before the value joins the
sample, and no `toml:` line is printed on that path. The bound landed at ~10 s
(54× the loosest ceiling) deliberately — "a few pipeline depths" is a *health*
threshold and would collide with the recorded ceilings, which already admit 8.01.
The wav capture cannot serve as the reference: its timeline is what soundd
submitted (3.32 s against 1130 submitted periods = 3.28 s), so a stall that
submits nothing does not lengthen it.

## The dropout regression: bisect, two wrong suspects, and the fix

Measured 2026-07-29 on tree `7095046`: 19 dropout runs in 76 config-runs (25%)
against the recorded 4 of 117 (3.4%), Fisher p=9.7e-4. Signature razor-sharp — in
all 19 kept captures the silence began at an *exact integer multiple* of the
23.219 ms pipeline depth after the tone's first sample (12 at 1.00x, 3 at 2.00x,
2 at 3.00x, 2 at 4.00x, never a non-integer, two decimals).

### Bisect (four same-session A/B sides, one host, HEAD's harness on every side)

| side | guest | pooled dropouts |
|---|---|---|
| A | HEAD `4562023` | 15/68 = 22.1% |
| B | HEAD, kernel+`toyos-sched` at `77dd5d1` | 15/64 = 23.4% |
| D | `9b1ba35` — the commit before the cutover | 2/48 = 4.2%, gate PASS |
| C | `6d6a230` — the tree the 3.4% baseline was recorded on | 0/80 = 0%, gate PASS |

A vs B p=0.65, so **Stage 7b was exonerated** — reverting both its commits' entire
kernel effect moved nothing. B vs D p=3.8e-3; pooled pre-cutover 2/128 vs
post-cutover 30/132, p=3.8e-8. Host exonerated: C measured 0/80 minutes after A
measured 15/68. The intermediate 7a states are not measurable — `98b8a02`
predates `eaffbf1` and curtails on counter ceilings, `eaffbf1` predates `8508b37`
and hits the 30 s `--smp 8` hang that commit fixed.

The signature was *created* by the cutover, not amplified: over all 268 kept
captures, pre-cutover 7 gaps with **0** on a ring-depth boundary (offsets 2, 2, 3,
29, 190, 193, 228 periods); post-cutover 203 gaps, **183 (90%)** on an exact
multiple of 8, 83% inside the first two ring-fulls.

### Mechanism, traced

soundd was not late — on every gapping run its wake lateness was 0.33–0.51
pipeline depths and `drains` was 0. The late party was the client:

1. The 440 Hz tone's phase is **frozen** across every gap. A least-squares phase
   fit either side of the silence matched "generator stopped" to <0.02 rad in all
   34 captures measured, never "audio generated and lost".
2. soundd's `underruns` equals the wav gap length in periods **exactly**, run
   after run — soundd read an empty slot ring and submitted the silence itself.
3. The gap starts at an exact multiple of 8 periods, which is `slot_count`
   (`= num_buffers`), the client's whole ring.

### Suspect 1, wrong: the RT boost window

Eliminated two ways. *Mechanism*: a temporary kernel probe counted every place
`RtState::expire` clears a lend, plus the maxima of a boosted task's Ready and
Running residencies. Across ~30 config-runs and ~20k lends the window **never
lapsed** — `expired d/preempt/park/pick = 0/0/0/0` on 235 of 236 reporting
windows, gapping and clean runs alike. It cannot lapse in steady state (soundd
re-lends every ~3 ms, window is 10 ms) and the work fits: 0.5–1.3 ms Running
residency steady-state, 8.1–9.5 ms for the stream-start whole-ring refill. So
"one quantum" was the right size, and an earlier finding that shrinking it hurt
was measuring the 8.5 ms refill, not the boost policy. *Rate*: same-session A/B,
60 first-boot config-runs per side, 18/60 vs 19/60, Fisher p=1.00. (Base rate
that session was 30%, not the 22% above — see the drift warning at the top.)

Two real defects were found on that path anyway and fixed (`9c2fc4d`,
`78b7bfb`); both are tails, not the rate. See
`specs/scheduler-migration-log.md`.

### Suspect 2, wrong: `underruns`/`drains` were "just counter artifacts"

The analysis was right and the conclusion was half wrong. `wakes` on
`audio_tone_load` smp=1 rising from a recorded 426–496 to 1050–1130 correctly
said soundd's mix cycles were now arriving before the client had refilled — the
CPU hog no longer starved it, which is B7 being fixed. But that was pointing at a
real defect underneath, not only at over-counting.

Three hypotheses tested and refuted along the way, worth not re-testing: the
run-queue tie-break (a real bug, fixed, changed nothing here); a shorter RT boost
window (worse — underruns 137 and an 8-period gap); removing the client boost
entirely (underruns unchanged at 102, but two real gaps appeared, so the boost is
load-bearing for audibility and is not what moves that counter).

### The fix (`069d158`) — in soundd, not the scheduler

`while free_mask != 0` drained every free DMA buffer in one mix cycle, and the
client's slot ring is `num_buffers` deep — §5.10's jitter margin — so one cycle
consumed the client's *entire* ring and then gave it a single signal-to-mix
window to regenerate a whole pipeline of audio (8.1–9.5 ms of work into a 5–7 ms
window at stream start). The margin was emptied in one pass and immediately
demanded back, which is exactly why every gap landed on an integer multiple of
the ring depth.

soundd now leaves a free buffer *unfilled* when a streaming client's ring is
empty — deferring, not blocking, so §5.10's "wait until clients have filled"
costs no reverse notification: the ring indices soundd already maps carry the
same fact. Deferral is bounded by `playout_until_ns`, a wall clock of when
submitted audio will have played out, because the free list cannot arbitrate this
— at stream start QEMU retires the whole pipeline 0.7–6.6 ms after a refill, so
"free" says nothing about what has been heard.

Same-session interleaved A/B, 20 `cargo test -- audio` invocations per side, 80
first-boot config-runs each: parent **19/80 (23.8%)**, fixed **0/80 (0.0%)**,
Fisher p=1.1e-6. `underruns` → 0 on all four configs, including
`audio_tone_load` smp=1 where the parent ran a median of 37 and a max of 53.
Below the pre-cutover baseline (3.4%), not merely below the regression.

`f4d8fa7` is still where the rate changed, but it *exposed* this latent soundd
defect rather than causing it: the cutover raised soundd's `wakes` 426–496 →
1050–1130, so ~2.4x more mix cycles cross the same refill and each is another
chance to drain the ring.

The `refill_floor_nanos` justification written with that fix was itself false and
was rewritten at `9ed8eda`: it claimed five periods covers soundd's worst wake
lateness of 12.4 ms over 76 config-runs, but the larger sample recorded 56909us
(19.6 periods) and no floor inside an 8-period pipeline could cover 2.45
pipelines anyway. The constant is policy, justified by measurement (0 dropouts, 0
underruns across 120 config-runs at `dc732e5`), not by that reasoning.

## Stage 7a-vs-7b counter comparison (superseded, kept for the drift lesson)

Same-session A/B, 5 invocations per side, 7a rebuilt from its own commit mid-7b
session. Pooled ceiling breaches: 7a 9 of 20 config-runs, 7b 7 of 20 —
indistinguishable at n=5. The `wake_lat` outliers (~55–65 ms, ~103–111 ms,
~200–215 ms, roughly 1:2:4) appeared identically at `-smp 1`, where there is no
sibling to migrate to, so "a task woken onto a busy CPU cannot migrate" was never
the mechanism. Every breach was in the `clients=1` window, mostly with
`underruns 0` and an empty gap histogram. All of it was instrument defect 2.

## The 30-second harness timeout that was not a guest stall

`run_test` matched `===TEST_END ` as a *line prefix*; soundd was mid-`println!`
when the runner printed the marker, so the marker landed inside soundd's line and
the harness never saw it. The guest had already exited cleanly. The marker is now
matched anywhere in the line and the preceding text is kept (it is usually the
stats line gate A reads).

Worth remembering as a class: a "guest hang" that only ever appears on the audio
tests is more likely to be the shared console than the scheduler. The underlying
writer-side defect — the virtio-console has no line atomicity — is still open;
see `specs/known-issues.md`.
