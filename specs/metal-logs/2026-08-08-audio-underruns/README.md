# The T14 after the wake fix: 686 underruns, and the rate the engine was really playing at

One boot of the owner's ThinkPad T14 Gen 2, 2026-08-08 17:57, on the tree that
carries `flush_log_file_if_affordable` (`727bcfb`). Five `tone` runs and one
doom session with music, HDA, full desktop. The file is the machine's own
`/log` and is the only channel it has.

## What the 54 stats windows total

```
windows=54 underruns=686 drains=5 completions=39202 submitted=39242
```

Against the boot before the wake fix (522 underruns / 375 drains, same
directory's sibling `2026-08-08-audio-wake/`): **drains are solved and
underruns are not.**

## The device was playing at 48 kHz

47 of the 54 windows are full-length. Their `completions` are 731, 740, 741,
749×5, 750×14, 751×21, 752×6 — median **751**. A window is `STATS_INTERVAL_NANOS`
= 2.000 s plus at most one loop iteration, so the period the engine actually
ran at is 2.000/751 = **2.664 ms**, and 128 frames in 2.6667 ms is 48000 Hz.
soundd generated every buffer for 44100.

The cause is `toyos_hda::stream::stream_format` putting the sample-base bit at
bit 13, where the Intel HDA stream-format structure puts the multiplier: 44.1
kHz encoded as `0x2011`, which is a 48 kHz base carrying a reserved multiplier,
where the spec's value is `0x4011`. Both `SDnFMT` and the codec's `Set
Converter Format` carried it, so the controller and the codec agreed with each
other and disagreed with soundd. Measured in QEMU on the same code path: the
`hda_tone` capture came back at **478.9 Hz** for a 440 Hz tone (440 × 48000/44100
= 478.91), and at **440.2 Hz** with the bit moved to 14.

Two things it had been hiding. `hda_tone`'s dither ratio read 3.2–3.3% against
an expected 25% — QEMU's audio core was resampling 48000 → 44100 into the wav
backend and smearing soundd's ±1 LSB dither away — and it reads 24.4–25.6% with
the fix, so **every earlier `hda_tone` measurement was taken through a
resampler**, the phase-break count of known-issues §4 / #88 included. And
`phase_breaks` cannot see the error itself: an 8.8% pitch error perturbs its
recurrence by ~12 LSB against a 400 LSB tolerance.

## The 2690 µs cluster is one device period

`max_wake_lat_us` over the doom windows: 2678, 2680, 2681×3, 2682, 2684, 2686×3,
2687, 2688×3, 2689×3, 2690×2, 2691, 2692×2, 2695, 2698×2, 2702, 2703, 2704, 2707
— a 29 µs spread over 30 windows, sitting 11–40 µs above the 2666.7 µs period
derived above.

It is a ceiling, not a typical wake, and the ceiling is soundd's own backstop.
The mix loop arms its wait on the DLL's prediction `t_est`. When the timer
fires a hair *before* the completion it was predicting, the cycle finds no
record, re-enters with `t_est` now in the past, and `target = t_est + k·period`
with k=1 — so the next wake it can have on its own is exactly one period later.
Anything that delays the completion CQE past that instant is reported as
"one period late" and nothing longer. After the rate fix the same mechanism
puts the ceiling at 2902 µs; **that is the number to read on the next boot.**

## The underruns are the client, and they happen while soundd is on time

Sorted per window:

| regime | `max_wake_lat_us` | `max_batch` | underruns per window |
|---|---|---|---|
| tone | 461–1130 | 1 | 0 0 0 0 1 4 8 11 12 14 16 16 16 24 27 |
| doom | 2678–2707 | 2 | 0 0 0 0 0 0 1 2 3 3 4 4 5 8 9 11 15 15 17 17 21 23 23 24 27 31 39 51 54 |

And the four windows where soundd *was* stalled — `max_batch=8`, wake latency
40,168 / 40,382 / 45,329 / 45,495 µs — report underruns **0, 0, 0, 23**. The
correlation runs the wrong way for a soundd defect: when soundd is late the
client has *more* time, and there is no underrun.

`max_batch=8` with `underruns=0` also settles the shape of the client ring:
soundd consumed eight client periods in one cycle and every one was covered, so
the ring is normally its full eight periods = 21.3 ms deep. An empty ring is
therefore a producer that stopped for at least that long, not a producer
running one period behind.

## What the four 40–45 ms events are

They are the same log-flush stall `2026-08-08-audio-wake/` documents, arriving
through `LOG_DEFERRAL_CEILING_NS`: the 1 s ceiling that stops "prefer a CPU that
owes nothing" becoming "never". Four of them in 122 s is the ceiling expiring
four times, which is the fix working as designed rather than a second defect.

One of them lands immediately after

```
[kernel 42.065 cpu3 tid=0] spawn: /bin/shell pid=16 … total=1ms
terminal: ready
soundd: … drains=2 max_wake_lat_us=45329 max_batch=8
```

which reads as "spawn holds a CPU". It is not: a spawn *emits a burst of log
lines*, and the burst is what the flush is flushing. The wake-fix README's own
table has the same shape twice — 39,194 µs after an 8-line `exit: tone` burst,
14,121 µs after a 5-line one.
