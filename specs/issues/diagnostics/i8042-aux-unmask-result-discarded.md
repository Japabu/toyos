---
status: open
kind: defect
opened: 2026-08-07
---

# The i8042 aux line's unmask result is discarded, and its log line says nothing either way

`init` captures the keyboard's unmask and prints it — `"on"` or `"MASKED"`
(`kernel/src/drivers/i8042/mod.rs:1527, 1544`). The aux line one statement
later is `let _ = ioapic::set_masked(l.gsi, false);` (:1529), and its log line
(:1547) reports the GSI, the vector and the APIC and stops:

```
i8042: kbd set2+xlat (readback 0x41) scanning on, GSI 1 -> vec 0x24 apic 0 on
i8042: aux rate=100 res=8/mm, GSI 12 -> vec 0x24 apic 0
```

An aux GSI that failed to unmask prints exactly that line. On the T14 that is
the TrackPoint and touchpad silently dead with a green-looking boot — the
failure mode the comment 30 lines above (:1494-1497, "every line below prints
green") was written to prevent, reached by the one path that does not check.
Not observed failing: this capture is QEMU with USB HID, and `i8042: armed at
191ms, idle at 335ms, 0 interrupts` is expected there. The point is that the
log cannot tell you which it was.

Trivial to fix the same way the kbd line already is.
