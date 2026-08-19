---
status: open
kind: defect
opened: 2026-08-07
---

# The T14's firmware hands over an *uninitialised* 8042 about one boot in seven, and the fallback has nothing to stand on there

Found across seven consecutive boots of one image, 22:26–22:33 on 2026-08-07.
Six read `cfg=0x77->0x64`. The boot logged at `222741` reads `cfg=0x30->0x60`, and the
driver disabled the keyboard by name:

```
i8042: kbd DISABLED - the set query answered 0xee and firmware's cfg 0x30 has
       translate off, so nothing says what the wire carries
```

**Subtract our own two commands and the number says more than the message
does.** `before` is read *after* `CMD_DISABLE_PORT1` and `CMD_DISABLE_AUX`, so
bits 4 and 5 of it are always ours. Firmware's byte was therefore `0x47` on the
six — kbd IRQ, aux IRQ, system flag, translate — and **`0x00` on the seventh**.
Bit 2 is the system flag POST sets to say it has initialised the controller. So
that boot was handed an 8042 in its power-on default: firmware had not touched
it at all.

The refusal is correct as written and this is not a request to weaken it. What
the entry records is that **the evidence the fallback rests on is not always
there**, on this machine, at a rate of roughly one boot in seven — and the cost
when it is absent is the whole of the machine's integrated input, because the
refusal returns before the aux port too. The owner sees a desktop with a dead
keyboard and a dead TrackPoint, and the one line saying why is in a log he can
only read after rebooting. In the 2026-08-07 set that boot is one of the five
"freezes", and it is not one.

Two smaller things fall out and neither is fixed:

- **The message calls `0x30` "firmware's own cfg" and two of its bits are
  ours.** The inference is untouched — bit 6 is firmware's — but the label
  overclaims, and the value a reader is asked to reason about is not the value
  the sentence names.
- **What a correct answer would even be is a policy question, not a bug.** The
  controller is put into translating mode by `wanted` regardless of what
  firmware left, so the open question is only what the *device* emits, and on
  this EC neither `0xF0 0x00` nor `0xF2` may be asked (see `t14-keyboard-will-not-report-its-scancode-set`).
  Guessing is the outcome the read-back exists to prevent; refusing costs the
  keyboard. It is the owner's call which way this machine should fail.
