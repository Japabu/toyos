---
status: open
kind: track
opened: 2026-08-02
---

# Every driver is still in the kernel, and moving one before the IOMMU is finished costs security

The owner's target is that `virtio` does not appear anywhere in the kernel. None
of the staged work toward it has started, and the strongest check on it is not
the driver files but the ABI: no syscall name may carry a NIC, GPU or audio
device operation. Today seventeen do.

**Ordering ruling, and it is not negotiable: the IOMMU lands first and
completely, interrupt remapping included, and no driver leaves the kernel before
it.** Moving a driver out without translation *costs* security — a descriptor
holding a physical address is an arbitrary read/write primitive over all of
memory. `issues/kernel/the-iommu-stops-at-translation.md` is that prerequisite,
and it is at translation only.

Four pieces are explicitly *not* blocked on that and can run in parallel:

1. **The kernel's audio registry is a concrete match on a device type.** The
   file this was scoped against has since been deleted, so this needs re-scoping
   before it can start; the GPU and NIC traits are the models to copy.
2. **netd is handed a writable mapping of the whole 2 MiB page containing the
   virtqueue descriptor, available and used rings.** This is a live isolation
   defect with the reproduction already written
   (`issues/isolation/netd-writable-virtqueue.md`), and it should land first
   regardless of everything else here.
3. **BAR sizing and re-assignment onto 2 MiB boundaries.** ToyOS maps at 2 MiB
   and nothing else, so both BAR mapping and DMA are 2 MiB-grained where every
   other IOMMU system is page-grained. Re-assign userspace-bound BARs; keep the
   overlap refusal as the assertion that it worked, not as the mechanism.
4. **The capability itself** — enumerating functions, reading config space,
   mapping a BAR, mapping DMA, receiving an interrupt. Adding syscalls needs the
   owner, and the tree has since moved the *other* way, growing a per-register
   read/write pair on a claimed device.

Two constraints that were not obvious before the code was read:

- **USB HID cannot move to userspace without moving the boot block device or
  splitting the controller.** It shares the controller, the event ring and the
  lock with the boot disk.
- The exception criterion is "a driver stays in the kernel only if the kernel
  needs it while userspace is dead". A widening to "if a *service the kernel
  itself provides* needs it while userspace is dead" — which keeps NVMe and
  changes nothing else — is waiting on the owner.
