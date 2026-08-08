---
status: open
kind: rejected
opened: 2026-08-01
---

# First-match device selection that remains, and why

`pci::enumerate` returns every function now, so a driver taking the first match
does so visibly. Two do, and both are deliberate:

- **NVMe.** `nvme::init` takes the first class-0108 controller. A machine with
  two NVMe drives loses the second, and there is nowhere to put it:
  `page_cache::init` takes a single `Box<dyn BlockDevice>`. Making this an
  enumerate-all is a storage-stack change, not a PCI one.
- **The four virtio drivers.** Each takes the first device with its
  (vendor, device) pair. A second NIC or a second GPU would be dropped. These
  are QEMU-only devices — no virtio function appears on the T14 — so the
  exposure is a test-shape one, and no profile declares two of anything virtio.

Neither is a defect today. Both become one the moment a second such device is
reachable, and the enumerate-all they would need now exists.
