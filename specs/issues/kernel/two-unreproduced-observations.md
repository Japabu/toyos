---
status: open
kind: finding
opened: 2026-08-01
---

# Two unreproduced observations

`ps` appeared to stall for >2 s under heavy single-core load; later runs fine. If
seen again, capture with LLDB before restarting.

Doom's music was heard once at roughly half speed. It did not reproduce at HEAD,
with or without `-nodefaults`, and the wav capture measured 1.00x — so whatever
happened, the device-side path was never wrong. Leading hypothesis is host
contention: another agent was building in this tree with a second QEMU running
at the time.

The durable part is the instrument, not the sighting. **Next time, read the
numbers rather than listening**: Doom prints `[music]` synthesis
real-time-factor telemetry every ~5 s, and soundd prints wake/underrun/latency
stats every ~2 s. A starved synthesizer and a wrong playback clock sound
identical to a human, and RTF is what separates them — RTF near 1.0 with the
audio still slow means the clock, RTF well below 1.0 means synthesis is not
keeping up.
