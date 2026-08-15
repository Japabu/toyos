---
status: open
kind: finding
opened: 2026-08-15
---

# `ps` appeared to stall for over two seconds under heavy single-core load

Seen once, never reproduced. If it happens again, capture with LLDB before
restarting — a stalled `ps` that has already exited leaves nothing to read.

This file used to carry a second observation, Doom's music heard once at
roughly half speed. That one is retired: it never reproduced at HEAD with or
without `-nodefaults`, its wav capture measured 1.00x, and the owner reports
(2026-08-15) Doom running well on the T14. The instrument it argued for outlived
it and lives in `specs/debugging.md`: audio that sounds wrong is read from
doom's real-time factor and soundd's stats, never from the ear.
