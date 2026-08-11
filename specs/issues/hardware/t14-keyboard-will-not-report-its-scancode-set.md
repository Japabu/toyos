---
status: open
kind: defect
opened: 2026-08-02
---

# The T14's keyboard will not report its scancode set, and one byte reached no event

The boot after the FADT gate came out reached the keyboard and stopped one step
from the end:

```
i8042: ok selftest=0x55 cfg=0x77->0x64 port1=ok port2=ok
i8042: kbd cmd 0x02 answered Some(238), not ack
i8042: kbd refused scancode set 2 ... disabled
```

238 is `0xEE`, ECHO's own reply, returned for the **argument** of `0xF0 0x02`
after the command byte was acked and after `0xF5` had been acked — so the EC
answers commands and does not implement this one. The driver now reads the set
rather than writing it, and where the read is refused it decides the wire format
from the translate bit firmware left in the config byte (`0x77` on this
machine), which is exactly what Linux's `i8042.c`/`atkbd` do and all they do on
a portable device. `i8042_kbd_echo` gates it.

**The boot after that one worked**, and it is the first time any of this has run
on the metal it was written for:

```
i8042: kbd set2+xlat (assumed, the set query was refused) scanning on, GSI 1 -> vec 0x24 apic 0 on
i8042: aux rate=100 res=8/mm, GSI 12 -> vec 0x24 apic 0
i8042: armed at 1460ms, idle at 3394ms, 0 interrupts ... the pin has never asserted (kbd GSI 1, aux GSI 12)
i8042: the pin asserts ... 1 interrupts, 1 bytes, 0 keys, 0 motion, first seen at 11375ms
```

The driver attaches; the **aux port initialises fully** — `rate=100 res=8/mm`
means the TrackPoint answered its whole reset/id/rate/resolution sequence, which
no previous boot reached because every keyboard-side refusal returns before that
block; and a physical keypress at 11375 ms raised a real interrupt on GSI 1, so
the routing, the RTE programming, the vector and the unmask are all correct on
Tiger Lake silicon. `Boot: peripherals ready` went from 6 ms to 398 ms in the
same boot: that is the aux reset stage now actually running against a device
that takes real time, not a regression.

What is open:

- **One byte reached the kernel and produced no event, and the counters could
  not say which byte.** That is the open item, and half of it is the
  instrument. Enumerated against the real tables (`toyos-ps2/src/key.rs`),
  **84 of the 256 single byte values decode to nothing** under set 1: both
  prefixes (`0xE0`, `0xE1`), the two `Lost` codes, and every unmapped slot —
  `0x54`, `0x55`, `0x59`–`0x80` and their break forms. `handle_key` drops a
  break for a usage nothing held, which adds `0xAA` (left Shift's break under
  translation, and a keyboard announcing a reset). So `1 bytes, 0 keys` covers
  an extended key where **nothing is wrong**, a late `0xFA`, another `0xEE`, a
  device reset, and a wire carrying raw set 2 — where Enter is `0x5A`,
  Backspace `0x66`, Escape `0x76` and 23 such codes land on unmapped slots.
  Only the byte separates them. The driver now records the bytes that produced
  no event and names them in the health line, and says it a second time if a
  later byte does decode; `i8042_undecoded_bytes` gates both. The next diag
  boot answers this in one line without a reflash.
- **The wire format is still the `assumed` one, so a raw-set-2 wire is among
  the suspects.** It is not the likeliest — a mismatch would usually produce
  *wrong* events rather than none, since most set-2 codes do land on a mapped
  set-1 slot — but 23 of them do not, and the byte value is what settles it.
- **The fallback's evidence is firmware's intent, not a read-back.** `before &
  CFG_TRANSLATE` says firmware enabled a set2→set1 translator; that it did so
  for a device emitting set 2 is inference, tight but inference. The success
  line says `(assumed, the set query was refused)` rather than `(readback
  0x41)` precisely so the panel does not claim otherwise. A machine where the
  inference is wrong types nonsense, which is the outcome the read-back exists
  to prevent — there is no third instrument for it on this wire.
- **`0xF2` is not that instrument.** A translating controller answers the MF2 id
  `AB 83` as `AB 41` (`translate_table[0x83] == 0x41`, QEMU `hw/input/ps2.c`;
  QEMU's own keyboard hardcodes the same pair), which would prove the translator
  is live on the data path and not merely enabled in a bit. It is not sent,
  because Linux's `atkbd_skip_getid` withholds `0xF2` from every translated
  portable device — "on many modern laptops ATKBD_CMD_GETID may cause problems"
  — and the T14 is one. Sending a command Linux avoids on this exact machine
  class to shore up an inference is the wrong trade.

- PCID + INVPCID codepaths untested on real hardware — QEMU TCG supports
  neither. Both are CPUID-gated, so TCG falls back to a CR3 reload. Needs KVM or
  bare metal.
- TLB shootdowns still IPI all CPUs for a full flush. Per-page targeted
  shootdowns not implemented.
- The LAPIC timer uses one-shot mode; it should use TSC deadline mode
  (`IA32_TSC_DEADLINE` MSR) for precise absolute-time wakeups. The TSC is already
  calibrated for `nanos_since_boot()`.

---
