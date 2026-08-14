# Device test build order

Stages for the coverage `specs/device-test-strategy.md` requires.

1. **Shape matrix.** Parameterize the profile machinery over device sets:
   no-USB-HID, four-USB-devices, no-NVMe, hotplug-after-boot,
   remove-under-load. Assert the boot reaches the compositor and the right
   daemons live or exit; a daemon whose device is absent exits — no panic,
   and no holding a service name with nothing behind it. Built so far:
   `Profile::MetalUsb` (six xHCI devices, two keyboards) and
   `Profile::MetalDisk` (the T14's exact NVMe namespace).
2. **Lifecycle.** `device_add`/`device_del` mid-run: hotplug, removal under
   active I/O, claim-then-die-then-reclaim. Nothing built.
3. **Storage ground truth.** Started: `nvme_large_device` writes a file
   in-guest, shuts down, and decodes both superblocks out of the image
   host-side with the kernel's own parser — the clean flag reaches the
   platter only through `PageCache::sync`, and the backup superblock only
   through a write at the far end of the device, so one assertion covers
   write-back and capacity at once. Remaining: the file's bytes, which needs
   a file-backed `BlockIO` so the harness can walk the btree rather than
   only the superblocks.
4. **Gate N.** Specified in `specs/plans/net-gate-plan.md`.
