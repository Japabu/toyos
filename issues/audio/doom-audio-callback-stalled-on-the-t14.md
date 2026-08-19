---
status: open
kind: defect
opened: 2026-08-05
---

# Nothing explains why doom's audio callback stalled on the T14

doom's sound producer can no longer kill the game when its callback stops
consuming — `doom_sound_flood` stages exactly that and the game lives — and
that is the whole of what is now known. **Why the callback stopped on the
owner's machine, about five seconds into play, is not.** The evidence is one
abort message, because the process that would have carried the answer is the
one that died.

An audio client's RT standing is *lent*, not held: soundd claims the audio
device, takes the RT band with it (`main.rs:705`), and every pipe write it
makes lends the woken reader a one-quantum window (`wake_pipe_readers`).
**On a machine with no audio device none of that happens.** The null
sink deliberately does not request the band — it protects no audible output —
so `driver::current_is_rt()` is false at its `signal_clients` write and the
client's callback thread is woken as an ordinary thread. The T14 has no audio
device. So the one thing that keeps a 2.9 ms deadline met was absent there and
is present in every config the suite runs.

That is a mechanism, not a measurement, and two others are equally live: the
`drain_irqs` entry, `issues/kernel/scheduler-pass-blocks-in-xhci.md`,
where any syscall on that thread can become the USB
driver's engine for as long as a second; and plain scheduling pressure from a
game thread and a compositor that never yield to it.

What would decide it is the callback's own period count against wall clock, on
that machine. doom now keeps that counter (`MIXED_PERIODS`) and now survives
the stall, so the next T14 session can be asked the question instead of losing
the process that would have answered it.
