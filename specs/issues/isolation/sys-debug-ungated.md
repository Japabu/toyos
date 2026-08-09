---
status: open
kind: defect
opened: 2026-07-31
---

# `SYS_DEBUG` is ungated, and two of its actions are a diagnostic-channel DoS

Action 3 — halt every CPU — no longer exists outside the `test-fatal-halt`
feature. The other three are still reachable by any process at any time, and
the audit that removed action 3 turned up what they cost:

- **0 and 1** (`panic!`, and a null read that faults in kernel context) each
  run a full `crash_report`: dozens of lines into the 64 KiB log ring, a
  `PROCESS_TABLE.try_lock()`, a kernel and a user backtrace with symbol
  resolution, and a `panic_flush` that drains the ring synchronously. A loop
  calling `debug(0)` therefore floods the one channel the kernel reports on and
  spends unbounded time in the panic path, and each iteration takes the
  recovery route, which
  `specs/issues/panic-path/panic-holding-process-table-hangs.md` documents as
  able to strand locks.
- **2** costs one lock permanently, by design, and is one-shot for that reason.

None of this is memory-unsafe and none of it kills the machine. It is a syscall
whose only purpose is to make the kernel misbehave, available to everything —
the same class as `SYS_SHUTDOWN` being ungated, and it wants the same decision:
a capability, or `#[cfg(debug_assertions)]`, or deletion.
