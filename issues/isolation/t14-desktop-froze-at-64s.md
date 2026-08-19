---
status: open
kind: defect
opened: 2026-08-03
---

# The T14 desktop froze at 64 s: the class is closed, the instance is not

The owner's machine went dead — no typing, no cursor — about 64 s into a
session, with the kernel log still streaming to the stick for another 9.6 s
until the power went off. That log is what prompted the SDK IPC-framing work.
What it establishes, and what it cannot:

**Established.** The compositor's 2 s report runs unbroken from 4.5 s to the
batch ending ~64.3 s and then stops, so the compositor stopped compositing
with ~5 reports missed. It did not panic (no backtrace, no `exit: compositor`
— the only panic in the log is toybox `tone`'s cpal `NotFound` at 29.5 s,
which is correct on a machine with no audio driver and 35 s earlier), and it
did not run out of memory (134 of 15404 MB, and the pool table is flat).

**One elimination the log does support on its own.** Every wait in
`Poller::wait` carries `FRAME_INTERVAL`, and the taskbar marks itself dirty
once a second, so a compositor parked in its poller still composites every
second and still reports every two. **It was therefore not in `poller.wait`.**

**Two more from the code rather than the log.** A blocking `write` to the
terminal needs its 2,097,088-byte receive ring full, which is 131,072 unread
messages; a whole session of typing and mouse motion is two orders of
magnitude short. And a blocking `recv_payload`/`recv_bytes` on the terminal's
window connection needs a payload-bearing message, while the only two the
terminal ever sends there — `MSG_PRESENT` and `MSG_DESTROY_WINDOW` — are bare
headers.

**What is left, and why none of it is proven.**

- `recv_header` on a freshly accepted connection needs something to have
  connected at ~64.3 s. The only connect the three surviving processes can
  make is `window::clipboard_set`, which the terminal calls on mouse-up after
  a selection — and the two batches before the freeze are 43 and 30 frames
  against a resting 4–6, with `composite_us_min` at 208 µs against a resting
  32 ms, which is cursor-sized damage: mouse motion over the terminal.
  Consistent, and not proven — `clipboard_set` writes its header into an empty
  2 MiB pipe in the next statement, so the compositor should have been woken.
- `accept` itself, on a listener completion whose queued connection was
  withdrawn. That needs a connector that dies, and nothing spawned or exited
  between `ps` at 50.1 s and the freeze.
- The drain-loop livelock, which needs a client whose fd is permanently
  ready. No producer for that among the three live processes.

**The measurement that would have decided it, and did not exist in time.** A
connection is two 2 MiB pipes allocated at `SYS_CONNECT`, and the PMM dump
counts them: `pipe held=5` at 64.348 s is exactly the compositor↔terminal
socket plus the shell's three tty pipes, so nothing had connected *yet*. The
dumps run every 10–13 s, the next was due around 74–77 s, and the log ends at
73.961. `held=7` would have named the accept path and `held=5` would have
ruled it out.

**What the next boot should capture.** Nothing new, which is the point: the
compositor now names every client it drops and why. A recurrence with no
`compositor: dropping pid` line and no telemetry is a mechanism none of the
four closed ones covers, and that is itself the finding. If it is worth
narrowing further before then, the cheap change is a `pipes=` field on the
compositor's own 2 s report — same cadence as the thing that goes missing,
where the PMM dump's is not.

Do not read the 9.58 s of no kernel output as evidence. It is the longest gap
in the log, but an idle desktop in this same session goes 4–6 s between
kernel lines routinely, and the scheduler lines that produce them come from
idle CPUs rather than from a heartbeat. 1.6× the normal gap is not a signal.
