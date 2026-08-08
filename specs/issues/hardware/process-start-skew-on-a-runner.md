---
status: open
kind: defect
opened: 2026-08-08
---

# A userland process reaches its first line half a second after its sibling on a runner, and ~30 ms after it here

Fell out of the probe above and is not what it closed. On all three reps the two
programs `tests/testcases` starts — soundd and test-runner, spawned 1–3 ms apart
— printed their first lines **0.53–0.56 s apart**, and which of the two was
first flipped between reps. On this host the same pair is ~30 ms apart, in spawn
order, every time. The kernel's own boot is the same speed on both (`Boot:
complete` at 275–304 ms on the runner against 269 ms here), so it is not a slow
machine: it is the first moment two runnable tasks exist.

The i8042 verdict measures the same thing from the kernel's side, since it is
emitted from the first idle-loop trip after arming: `idle at` 523–552 ms on the
runner against 304 ms here.

Nothing here says whether that is the host descheduling a vCPU thread, something
in userland startup, or the scheduler leaving a task unclaimed — the probe was
not built to tell them apart. It is recorded because a half-second of skew
between two init children is enough to decide any remaining wall-clock margin in
the suite, and because it is invisible on a host whose TCG runs one vCPU at a
time.
