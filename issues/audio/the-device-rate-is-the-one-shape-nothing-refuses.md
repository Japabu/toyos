---
status: open
kind: finding
opened: 2026-08-20
---

# `Shape` names every constraint the mix loop imposes except the sample rate

`toyos_mixer::period_frames` refuses a device by name for a pipeline that is too
shallow, too deep or not a power of two, for a channel count that is neither
mono nor stereo, and for a period that is not a whole number of frames. Its
header says that is *every* constraint the mix loop's own arithmetic imposes.
It is not. Two more ride on `device_sample_rate`, and neither is checked:

- `toyos_mixer::period_nanos` divides by it. A rate of 0 is a divide-by-zero in
  `mix_thread` and in `control_thread`, which is a panic that names neither the
  device nor the reason — exactly what the `Shape::Channels` arm was added to
  stop happening for a zero channel count.
- `toyos_mixer::ramp_frames` is `rate * 5 / 1000`, so any rate under 200 Hz
  gives a ramp of **zero frames**, and `GainRamp::set_target` then divides by
  zero. It survives — `remaining` is 0, so `next` never applies the infinite
  step, and `advance_frames` produces a NaN that the `remaining == 0` arm
  overwrites with the target on the same call — but it survives by accident, on
  three separate arms all having to line up, not because anything refuses it.

**Not reachable today, and that is the whole reason this is a `finding` rather
than a `defect`.** Every rate that reaches the mix loop comes from a closed
table this repository writes:

- `userland/soundd/src/virtio.rs`'s `SUPPORTED_RATES` is `[(44100, …),
  (48000, …)]`, and `choose_params` picks one of those two or refuses the
  device; the device's own rate bitmap is only ever *matched against* them.
- HDA uses `toyos_hda::config::RATE`, the constant 44100.
- The null sink uses `NULL_SINK_RATE`, the constant 44100.

So the value is never a number a device chose. It becomes one the moment a
backend negotiates a rate rather than selecting one — which is what a device
offering 96 kHz would require — and the refusal structure that exists precisely
so a surprising device gets the null sink instead of killing the daemon would
not cover it.

The fix is one arm and one check: `Shape::Rate(u32)`, refused for anything that
does not leave `period_nanos` and `ramp_frames` both non-zero, with
`period_frames` taking the rate as an argument so no caller can do the
arithmetic before asking. `toyos-mixer/src/shape.rs` is where both live and
where the host test would go.

Found while extracting `toyos-mixer/` from `userland/soundd/src/main.rs`; not
fixed there, because a new refusal arm changes which machines get the null sink
and that is a behaviour change rather than an extraction.
