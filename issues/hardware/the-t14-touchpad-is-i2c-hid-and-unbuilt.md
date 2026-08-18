---
status: open
kind: track
opened: 2026-08-01
---

# The T14's touchpad is I2C-HID and nothing drives it

The metal milestone's stated completion condition is the real touchpad working —
the owner's ruling is that a PS/2 fallback with no multitouch does not count.
Nothing exists: no LPSS I2C controller driver, no ACPI `GpioInt` handling, no HID
multitouch parsing.

**It is debuggable only on the machine.** None of that hardware can be emulated,
so this is hostage to session availability and to the on-screen console staying
readable. It is blocked on nothing else in the tree.

Machine facts it will need, none of which QEMU can supply:

- ThinkPad T14 Gen 2, i5-1135G7 Tiger Lake, 16 GB, 256 GB NVMe, **no SuperIO** —
  the 16550 loopback reads `0xFF`, so there is no serial console and the inline
  log drain is a branch never taken on that machine.
- Its FADT is revision 6 and reports `iapc_boot_arch = 0x0011`: legacy devices
  set, **the 8042 bit clear**, no-ASPM set. The table contradicts itself, so that
  bit must not gate the i8042 probe; a floating-bus `0xff` from port 0x64 is the
  real absence test.
- The keyboard will not report its scancode set: the argument byte of the query
  answers `0xEE`, which is the echo command's own reply. The driver reads the set
  and never writes it, falling back to the firmware's translate bit — `0x77` on
  this machine.
- Metal boot is about **1151 ms against QEMU's 196 ms**, roughly 5.9×.

Three T14 defects are open beside this one:
`issues/hardware/t14-lost-every-integrated-input.md`,
`issues/hardware/pulling-the-boot-stick-freezes-the-t14.md`,
`issues/hardware/t14-keyboard-will-not-report-its-scancode-set.md`.
