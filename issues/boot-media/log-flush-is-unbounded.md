---
status: open
kind: defect
opened: 2026-08-02
---

# The kernel log's flush is unbounded, uninterruptible, and in front of the scheduler pass

Not closed, and it is the residual under gate A's red run. `idle_loop` is
`drain_serial(); log_file::poll(); pass()`, so a wake that arrives while a CPU is
inside the flush waits for the whole filesystem write plus a device cache sync
before any pass can dispatch it. `wait_transfer` spins, and `Lock` holds off
preemption, so nothing shortens it.

Measured in-guest: **7.2–26.0 ms per flush** before the resident-block change,
against a DMA pipeline depth of 23.219 ms — a single flush could empty the
entire audio pipeline. After it, 2.0–9.7 ms, which is what let gate A pass, and
still a third of a pipeline at the tail.

Two premises in `log_file`'s own documentation do not hold, and both are worth
carrying:

* *"It costs nothing when nothing is logged."* True of `log!`, and the ring is
  shared with **userland console output** (`SerialWriter::console` →
  `log_ring::write_chunk_blocking`), so every `println!` any process makes is a
  device write from the idle loop. soundd's own 2-second stats line is one.
* *"A busy machine reaches the idle loop rarely, so each flush carries more."*
  A busy machine has idle *CPUs*: at `--smp 8` seven of them are in this loop,
  and at `--smp 1` the machine is idle between audio periods. The one gate A
  config that did **not** regress was `audio_tone_load.smp1` — the only one
  whose single CPU is never idle. That fingerprint is what identified the
  module.

What it would take to remove rather than shrink: the flush has to become
resumable, or move off the idle path into something the scheduler can preempt.
Both are design decisions and neither is a bounds check.
