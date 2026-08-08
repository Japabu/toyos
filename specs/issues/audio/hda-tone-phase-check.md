---
status: expected-red
kind: defect
opened: 2026-08-07
task: 88
---

# HDA: the captured tone is not one sine

**This is the write-up `tests/toyos.rs`'s `EXPECTED_FAILURES` names for `hda_tone`.** Do not delete it without moving that pointer.

`hda_tone` plays the same 3.0 s 440 Hz tone the virtio arm plays, out of an
`intel-hda` controller soundd drives itself, and the capture comes back with
**8 to 16 phase discontinuities** where the virtio arm has none. Declared in
`EXPECTED_FAILURES` against the message "the captured tone is not one sine";
every other assertion that test makes still reds the run.

What is *not* wrong, measured on this host (QEMU 11.0.3, 2026-08-07): the tone
is present at full amplitude, there is **no mid-tone silence at all** (`gaps
none`), and soundd's own counters match the virtio arm's — **1127 periods
submitted, 0 underruns, 0 drains** on both. The guest put the same audio on the
wire; something between soundd's buffer and the wav file did not carry it.

The instrument is new and calibrated: `audio::phase_breaks` tests the recurrence
`x[n+1] = 2·cos(ω)·x[n] − x[n−1]` that a sampled sinusoid obeys exactly, and it
reads **0 on all four recorded virtio configs** (`audio_tone` and
`audio_tone_load`, smp 1 and 8). It is `specs/hda-driver-plan.md` §5.3 item 5's
second guard, built because `specs/hda-driver-plan.md` §2.4's zero-on-complete rule — the thing that keeps
one gap detector valid for both backends — is a design promise with no
measurement behind it (risk 7).

Evidence, and why it does not yet name a cause:

- Six consecutive runs at `timer-period=5000` gave **8 breaks at identical frame
  positions** — 2703-2705, 2821-2823, 2939-2940 — across runs whose audio
  content differed (the dither seed is clock-derived, and the sample values at
  those positions differ run to run). Identical positions with different content
  is a capture dropping samples on a cadence, not a guest playing them wrong.
- Shortening the host's drain interval to `timer-period=1000` moved the
  positions rather than removing them: one run at 0 breaks, the next at 16,
  clustered around frame 95725 instead. **So it is intermittent and the host
  cadence is not the whole story.**
- Within a cluster the breaks sit at multiples of **118 frames**, which is
  neither the device period (128) nor either backend timer's frame count
  (44.1 or 220.5). That number is unexplained and is the sharpest thing here.
- The capture also holds **2.756 s of tone where the virtio arm holds 2.94 s**,
  with no seam accounting for the difference on the runs that show none. Either
  the capture opens late or QEMU's `hda-codec` discards on its own ring
  overrun; both are host-side and neither is established.

Where to start: QEMU's `hw/audio/hda-codec.c` output ring against soundd's
eight-period pipeline. **Do not weaken the check to make it green**; the virtio
zero is what says it has teeth.

**2026-08-07: one guest-side cause found and fixed, and it is not the whole of
it.** soundd filled a completion batch lowest-index-first, so a batch that
wrapped the ring — `{6,7,0,1}` — was filled 0, 1, 6, 7 and played 6, 7, 0, 1.
That is a splice with no silence in it, which is this signature exactly. Six
`hda_tone` runs in one session, instrumented with a counter of fills that were
not in the engine's order, separated cleanly: **one run at `out_of_order=2`,
`max_batch=7`, 9 phase breaks; five at `out_of_order=0`, `max_batch=4`, 0
breaks.** It is fixed and the fill order is now the
engine's by construction.

**The breaks survive it.** Two runs on the fixed tree gave 8 and 6, with
`deferred=0` and an ordering that can no longer be wrong. So the ordering was a
contributor and not the cause, and the remaining one is still unnamed. What the
new numbers add:

- The break count tracks **soundd's own wake lateness**, which on this host is
  the host descheduling QEMU: 0 breaks at `max_wake_lat_us` 8626–8752 (five
  runs), 8 at 22525, 9 at 16730, 6 at 50837. Nothing in the guest changed
  between them.
- soundd put **1127 periods = 144,256 frames** on the wire and the capture holds
  **131,061** — 9.1% of what was submitted is not in the file, with `gaps none`
  and `underruns 0`. On the run at 6 breaks it is 10.8%. A capture missing a
  tenth of its samples has phase breaks whatever the guest did.

So the next step is the host side. `isr_complete` is a weaker candidate than it
was: `stream::decode` now refuses any mask that is not a walk of the ring, and
