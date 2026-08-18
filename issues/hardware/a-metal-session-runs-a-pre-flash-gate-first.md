---
status: open
kind: track
opened: 2026-08-01
---

# A metal session runs a pre-flash gate first, and the gate is a written verdict

The T14 has no serial: the 16550 loopback reads `0xFF`, so the on-screen console
is the entire diagnostic channel and anything it cannot show is silent. Flashing
a bad image costs a session. This is what a session does before it flashes, and
it is a *written* verdict — pass, fail, or read-verified-only with the reason —
frozen with its date, not a checklist someone ticks.

**Run it on a quiet tree. No-go on any unresolved false pass, even where the
command printed success.**

Audit the delta first: every commit to the kernel, bootloader, build system, ABI
and SDK since the last such verdict, by hash, with a "boot is unchanged" item for
anything touching the boot path.

1. **Nothing may write to a disk it was not given.** The highest-consequence
   section and the one the harness cannot catch. Read the designation stamp's
   definition and every hunk of history touching it and the filesystem adapter.
   Establish that format and mount are not public and that every raw block write
   is downstream of the probe — by *enumerating callers*, not by matching three
   names, and remembering that an enumeration which skipped the cargo git
   checkouts is not an enumeration. *False pass:* a new caller reaching a write
   path without consulting the stamp, or a check that warns and continues.
2. **The image is flashable.** Measure the built file's size **on disk** and
   confirm it is a whole number of 512-byte sectors — the build's own assert
   covers the computed size, not the file. Confirm `EFI PART` in the *final*
   sector: a healthy primary GPT hides a missing backup.
3. **Boot-time panics stay closed** — for each, the guard exists *and* a test
   exercises the absent-device path. A diskless boot. The required-CR4
   declaration. The framebuffer extent computed from stride, not width — on QEMU
   they are equal, so read the expression. An xHCI boot with zero HID devices,
   confirmed from the log line rather than from the return type. Expect two of
   these to be read-verified only: TCG reports every CPU feature present, so the
   missing-bit path cannot run.
4. **The on-screen console. If this fails, do not flash.** Every screen test.
   Confirm the muted profile actually removes the UART, and that the paging test
   is driven by a timer rather than a keypress — input may be dead on the
   machine. *False pass:* the late-panic gate passes with the capture routine's
   body replaced by a bare return, so these cover rendering, not capture.

**State to the owner before he boots**, because it is the difference between a
diagnosis and an afternoon: a refusal to attach to the keyboard is the driver
working, not a regression; the touchpad is I2C-HID and unbuilt, so a dead
touchpad is the expected outcome and must not consume debugging time; and TCG
cannot measure the 2× bar, so performance is what the session is for.

The session checklist itself — the measurements only silicon can close — is
carried here too:

| measurement | closes |
|---|---|
| one boot with AP control-register inheritance armed against one without, same image, same session; record the delta | `issues/kernel/ap-control-registers-inherit-init.md` |
| transcribe the 16550 loopback line, and the boot's own completion time beside the last metal-sim reading of the same image | the T14 half of the console-drain question |

`issues/hardware/pre-flash-gate-missed-the-milestone.md` records that the last
verdict certified everything except input.
