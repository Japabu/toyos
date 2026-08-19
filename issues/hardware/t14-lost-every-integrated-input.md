---
status: open
kind: defect
opened: 2026-08-03
---

# The T14 lost every integrated input at 6.6 s, and the log cannot yet say why

All three integrated pointers and the keyboard are behind the one i8042, and all
three went dead 6.6 s into the 2026-08-03 compositor session. The whole of what
the driver said about it, and the last `i8042:` line in a 58-second log:

```
[kernel 6.594 cpu0] i8042: 1 interrupts and 1 bytes, nothing decoded — no event from [aux 0x08], first seen at 6594ms
[kernel 6.609 cpu1] i8042: the pin asserts — 6 interrupts, 6 bytes, 0 keys, 2 motion, no event from [aux 0x08, aux 0x06, aux 0x08, aux 0x0e], first seen at 6594ms
```

**That line does not say what it looks like it says, and the first task on it was
opened on the strength of the misreading.** `0x06` has bit 3 clear and no packet
head ever does, so the four listed bytes read as a framer that had lost the
frame. They are not. Six bytes, two motion events, four bytes named: 2 × 3 = 6,
and the four are the head and first body byte of two whole, correctly framed
packets — `0x08` is a resting head and `0x06` is a `dx` of +6. **The pointer was
framing perfectly.** The arithmetic is forced and no reader would do it.

Closed, therefore: the decoder did not desync, and no fix for a desync was
needed. What was wrong is the instrument, and it is now fixed (`647c3c0`,
`toyos-ps2`) — `MouseOutcome` could not distinguish a byte held inside a packet
from a byte thrown away at a boundary, so two of every three bytes of a healthy
pointer stream were reported as suspects. `i8042_mouse` now runs three thousand bytes of
clean packets and requires the driver to name none of them; reverting the split
reds it with the T14's own line shape.

**What remains open is the actual question, and the log cannot answer it.**
The tally counts in the ISR before any decoding, so 6 interrupts is hardware truth
— but it is truth *as of 6.609 s*, which is when the driver stopped speaking.
`HEALTH_DONE` was terminal. For the remaining 54 s the log cannot separate:

- **the pin stopped asserting** — a wedged controller, a lost edge, an EC that
  stopped scanning, an RTE that got masked; from
- **bytes kept arriving and decoded to nothing** — a wire-format or framing
  fault, in this driver.

Those are opposite defects in opposite subsystems and the counters that tell
them apart were read once. Two facts are established and neither settles it: all
six bytes were aux (four named `aux`, two produced motion, `0 keys`), and **the
keyboard produced no byte at all in 58 s** — not "stopped at 6.6 s", never. The
same machine's earlier boots drove a shell off that keyboard (`metal-hardware-
inventory.md`), so it is not a routing fault.

The cadence fix is what makes the next session decisive rather than a guess:
after the verdict the counters repeat, at most once per 10 s and **only when the
pin has asserted since the last line**. That gating is the point — past the
first repeat, no line means no interrupt, so silence becomes evidence instead of
absence of evidence. `i8042_health_cadence` gates it, and reverting either half
(fire on the timer, or make `HEALTH_DONE` terminal again) reds it at 9 lines and
0 lines respectively against the required 2.

**What the next boot should capture.** A repeat line dated after 6.6 s, or none.
If bytes are arriving, `undecoded`/`discarded` name the fault in this driver. If
no line appears at all, the pin is not asserting and the next suspect is the
controller or the EC — and nothing in `toyos-ps2` can be responsible.

Two things deliberately not concluded:

- **The touchpad is not evidence of a mux problem.** The T14's touchpad is
  I2C-HID off an LPSS controller that is not on the PCI bus at all; the EC
  mirrors it onto the aux port beside the TrackPoint. The aux device answered
  `0xF2` with id `0x00` — a plain 3-byte mouse — so the driver's 3-byte frame is
  what the wire carries, and the 4-byte IntelliMouse mismatch usually blamed for
  a PS/2 desync is not available here.
- **The USB mouse plugged in at 30.4 s produced no motion either**, which is the
  xHCI HID completion-requeue item in this section and not this one.
