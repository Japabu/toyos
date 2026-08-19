---
status: open
kind: track
opened: 2026-08-03
---

# HDA plays, but there is no jack detection, no volume control and no volume keys

The driver is built and audible on both QEMU and the T14: codec decoding and
output-path selection are a pure crate, the register allowlist is in the kernel
with soundd holding no physical address, and three guest gates stand on it. What
was staged after that never started, and it is about 850 lines of greenfield
blocked on nothing:

- **Jack detection and routing.** No pin-sense verb exists anywhere. A poll on
  the presence bit, a switch between the fixed speaker and the headphone pin,
  and a ramp across the switch so it is not a click.
- **Master volume and mute**, at the codec rather than in the mixer, and the
  message to set it.
- **Volume keys.** The premise was never established either — the diagnostic
  that would say what the T14's keyboard sends for them was never committed.
- **Persistence** of the chosen output and level, blocked on the kernel keeping
  anything it enumerates (`issues/diagnostics/the-kernel-keeps-nothing-it-enumerates.md`).

**Two pieces of the shipped work are still owed a verdict.** The four HDA
sections of the audio baseline were never recorded, so the HDA arm asserts harm
and claims no distribution and has no thorough tier — that is 30 invocations on
a quiet host. And two shape profiles are missing: a codec with no speaker pin,
and a controller with no codec. Without them the refusal-to-null-sink path is
asserted only on a machine that has no controller at all.

Constraints worth not re-deriving (each already stated at its site in
`toyos-hda/` or `kernel/src/drivers/hda.rs`, listed here because they are what
the next pass will want):

- **The T14's trap is pin 0x1b, not the display codec.** Four pins claim
  "speaker" with no physical connection; only port connectivity separates them,
  and the display codec happens to sit at the higher address, so first-match
  wins there by luck.
- **EAPD is not optional** — both T14 output pins are EAPD-capable and read the
  bit clear at boot, so the obvious bring-up sequence plays silence.
- **Mute is at the pin, gain at the converter.** The T14 converter amp is 88
  steps of 0.75 dB with no mute bit; both pin amps are mute-only, one step.
- **The stream-format sample-base bit is 14, not 13.** `0x2011` is a 48 kHz base
  with a reserved multiplier that both controller and codec accept — and play
  8.8 % fast.
- **A cyclic HDA ring is not a free list**: an unrefilled period is replayed and
  completed twice, which is why the released buffer is zeroed.
