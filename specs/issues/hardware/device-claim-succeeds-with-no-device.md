---
status: open
kind: defect
opened: 2026-07-31
---

# A device claim succeeds on a machine that has no such device

`device::try_claim` gates `DeviceType::{Framebuffer, Nic, Audio}`
on an info struct the driver registered, so those three return
`ClaimError::Absent` when the hardware is absent — which is what makes soundd
and netd able to exit cleanly.
`DeviceType::Keyboard` and `DeviceType::Mouse` are gated on nothing at all: they
hand out a handle whether or not any driver will ever produce an event. Under
metal-sim the compositor holds both claims on a machine with no HID of any kind
and polls them forever. Wrong in the same way the isolation
issues in `specs/issues/isolation/` are wrong: a claim is supposed to be evidence.

## Capability endowment made the consequence worse and made it visible

Recorded 2026-08-10 by the adversarial review of `wt/toyos-endow`, and the two
halves are separate facts.

**Worse.** A claim used to be minted by whoever wanted it, so a `Keyboard` claim
on a machine with no keyboard cost that daemon a poll of nothing. Now `/bin/init`
mints every claim the manifest declares, before the program runs, and endows it
— so the machine's one holder of `Rights::DEVICE` mints authority over hardware
that does not exist, for a program that then holds a working handle to nothing.

**Visible.** `/bin/init`'s `start` prints `init: <program>: no <class> on this
machine` for every class `SYS_DEVICE_CLAIM` refuses, and that line is now how a
daemon learns what it got — soundd's "did I get an HDA or a virtio-sound?"
became "which claims are in my endowment table?". For `keyboard` and `mouse`
that line **can never fire**, because `device::try_claim` never refuses them. On
the T14, the machine where it matters — `t14-lost-every-integrated-input`,
`t14-hands-over-an-uninitialised-8042` — the compositor's endowment table is
indistinguishable between "the input devices are there" and "there are none",
and init's log says the same thing either way.

The capability endowment migration considered gating `keyboard` and `mouse`
the same way and did not: inventing a presence signal for two input classes
is its own piece of work, and no part of that migration needed it.
