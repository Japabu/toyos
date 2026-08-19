---
status: open
kind: track
opened: 2026-08-02
---

# Device shape and lifecycle have no coverage, and shape comes before protocol depth

Every device test in the estate runs against one machine shape, with every
device present and none arriving or leaving. The ground truth a device test owes
is at the hardware boundary: what the guest did to the device, read back from
the device — a captured wav, a pcap of the virtual wire, an image decoded
host-side with the kernel's own parser — not what the guest said it did.

**Order, and it is deliberate: shape and lifecycle before protocol depth.** A
daemon whose device is absent must exit cleanly — no panic, and no holding a
service name with nothing behind it — and that is worth more than another layer
of protocol conformance.

1. **Shape matrix.** Parameterise the profile machinery over device sets:
   no-USB-HID, four-USB-devices, no-NVMe, hotplug-after-boot, remove-under-load.
   Assert the boot reaches the compositor and that the right daemons live or
   exit. Two shapes exist (the T14's six-device xHCI, and its exact NVMe
   namespace); the rest do not.
2. **Lifecycle.** `device_add`/`device_del` mid-run: hotplug, removal under
   active I/O, claim-then-die-then-reclaim. Nothing built.
3. **Storage ground truth.** Started — one test writes a file in-guest, shuts
   down, and decodes both superblocks out of the image host-side with the
   kernel's own parser, so one assertion covers write-back and capacity at once.
   Remaining: the file's own bytes, which needs a file-backed `BlockIO` so the
   harness can walk the btree rather than only the superblocks.
4. **The network gate** — `issues/build/there-is-no-network-gate.md`.
