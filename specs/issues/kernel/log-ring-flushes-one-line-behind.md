---
status: open
kind: defect
opened: 2026-07-31
---

# On an idle machine the log ring flushes one line behind

Measured while building M2's i8042 tests. With no userland process doing
anything, a `log!` line reaches the console only when the *next* piece of work
wakes a CPU — so the most recent line is always still in the ring. Injecting
keystrokes 200 ms apart into an otherwise idle guest and watching serial:

```
0.144  i8042: drain bytes=2 keys=2 woke_kb=1     <- key 'a'
0.347  i8042: drain bytes=6 keys=0 woke_kb=0     <- Pause
0.551  i8042: drain bytes=2 keys=2 woke_kb=1     <- key 'b'
0.754  i8042: drain bytes=2 keys=2 woke_kb=1     <- key 'c'
                                                 <- key 'd' never appeared
```

`drain_chunk_to_serial` runs from the idle loop and the timer path, and an idle
CPU that has just finished its work does not come back for the line it queued
on the way out. The consequence for anyone reading serial: **the last line
before a quiet period is not evidence of anything**, and a guest that wedges
right after logging its final line looks like it never logged it. Every
existing test happens to keep the guest busy, which is why this has not bitten
before; `i8042_no_spurious_wake` drives an in-guest reader for exactly this
reason. Fix is a flush on the transition into idle, not another poll.
