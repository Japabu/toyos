---
status: open
kind: track
opened: 2026-08-02
---

# The IOMMU translates, and does none of what it exists for

Discovery and translation are built: every unit the machine reports is
inventoried, every function `pci::enumerate` returned gets a context entry
naming one identity-mapped domain, the fault event is armed, translation is on.
A unit this kernel cannot program is left switched off with a line naming the
register that decided it, which leaves that machine exactly as it boots with no
unit at all. Three gates stand on it, two of which strand a function above and
below its context entry — the only things in the suite that can tell a
translating unit from one that is merely switched on, because under identity
mapping every device *in* the tables behaves the same either way.

Interrupt remapping, per-driver domains and the refusal are not built. They land
in this order and each leaves the tree green.

**Interrupt remapping.** Remappable I/O APIC redirection entries and remappable
MSI/MSI-X, `IRE` set and `CFI` clear, every IRTE source-id-verified. This is the
step that can black-screen the machine: enabling `IRE` changes how *every*
interrupt message is interpreted, so every source already programmed —
the i8042's two I/O APIC lines, and xHCI, virtio-net, virtio-sound and HDA,
which all write the compatibility-format address that `CFI=0` blocks — must be
reprogrammed first. It lands alone, against a suite where every input, storage
and audio test already depends on an interrupt arriving. Two things to *verify*
on the first boot rather than assume: that QEMU blocks compatibility-format
messages once `CFI` is clear, and that the unit's own fault-event MSI is exempt.
A fault channel that stops working the moment faults become possible is the
worst version of this bug — which is why the unit is armed before translation
is enabled today.

**Domains, mapping, invalidation, faults.** Create/attach/map/unmap/flush, an
IOVA allocator, and the half of the fault handler that kills a process instead
of halting the machine: Bus Master Enable cleared on the offending function
(which is what lets the handler stay bounded once it no longer stops the
machine), a first-fault latch, a counter, a per-domain flag, the reschedule
handoff, and a storm ceiling. The portable seam grows here too — with one domain
and one backend, a domain id, a permission set, an error type and an `Iommu`
trait would each have a single value or a single implementor, which is why none
of them is in the tree. What is not negotiable is that teardown's slow half
never runs from the deferred zero-handle queue.

**The refusal.** Sequenced last, after the first userspace driver has moved
(`issues/kernel/every-driver-is-still-in-the-kernel.md`), because before that it
costs every machine with no vIOMMU — the default for QEMU, VirtualBox, VMware,
Parallels and essentially every cloud instance — and protects nothing that has
moved. Two rules need restating before they can be written, because as worded
they would refuse the harness's own machines: the extended-interrupt-mode rule
has to be stated in terms of the x2APIC ids actually in use rather than of
x2APIC being enabled, and the isolation-scope rule refuses a device whose scope
is not a singleton — written for peer-to-peer behind a switch, and wrong for a
root-complex-integrated function, which is what both the audio and networking
targets on the T14 are. Restating it is the owner's call.

Risks that are not stage-specific:

- **Whether the harness's virtio devices are behind the unit is unknown.** QEMU
  bypasses the unit for a virtio device unless it is created with
  `iommu_platform=on`, which requires the guest to negotiate
  `VIRTIO_F_ACCESS_PLATFORM` — these drivers do not. Under identity mapping the
  two are indistinguishable, so both isolation gates today run on ordinary
  emulated PCI functions. A host flag the guest silently declines is a vacuous
  gate: assert the negotiation guest-side.
- **Isolation scopes and reserved regions are modelled, not measured.** QEMU's
  topology is flat and publishes no RMRR; the T14 gives the first real answer,
  and it may refuse a device this project wants in userspace.
- **Cost is unmeasurable in the harness.** The 2× bar is answerable only on
  hardware, in a same-session A/B. So are cache-snooping walks, real
  access-control enforcement, and mid-DMA function reset on a real device.
