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
hand out a `Descriptor` whether or not any driver will ever produce an event. Under
metal-sim the compositor holds both claims on a machine with no HID of any kind
and polls them forever. Harmless today, wrong in the same way the isolation
issues in `specs/issues/isolation/` are wrong: a claim is supposed to be evidence.
