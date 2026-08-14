# IOMMU

## 1. Definitions

- A **unit** is one hardware remapping unit, discovered from firmware's
  declaration.
- A **domain** is one device address space: a set of translations the unit
  enforces for the devices assigned to it.
- A **device address** is what a device emits on the bus; it is meaningful
  only within that device's domain.
- A driver **gives** its device memory through the kernel's map interface;
  the given set is exactly what is currently mapped in the device's domain.

## 2. Requirements

1. **A device can reach only the memory its driver gave it.** A device whose
   driver is a userland process cannot read or write any page not currently
   mapped in its domain — not the kernel's, not another process's.
2. **A device can raise only the interrupts assigned to it.** Interrupt
   remapping is enabled together with DMA remapping, never after it: an
   interrupt is a memory write that DMA remapping does not see.
3. **A page unmapped by a driver is unreachable by its device before the
   pages are reusable.** Unmapped pages return to the allocator only after
   the unit has acknowledged an invalidation covering them; an
   acknowledgement that does not arrive within the fixed bound is a panic.

## 3. Boot requirement

A machine with no usable unit does not boot; there is no configuration that
runs drivers unprotected. **Usable** means all of: the unit is declared by
firmware and decodable; it supports interrupt remapping, an implemented
address width, 2 MiB pages, queued invalidation, and a domain per device on
the machine.

The refusal is a halt with the reason on screen and serial — not a panic, no
backtrace. Every refusal message states which condition failed and the raw
register value it was decided on where a register was read; where the user
can act, it names the firmware setting. A missing firmware declaration is
reported as indistinguishable between absent hardware and hardware disabled
in firmware, because it is.

Firmware's declaration is untrusted input: a structure or device scope
declaring a length its parent cannot hold ends the walk at that element,
reported with the offset and the claimed length — never a panic, never a
resynchronised walk.

This refuses AMD machines, ARM64 machines, Intel machines without VT-d or
with it disabled, and virtual machines whose hypervisor exposes no vIOMMU.

## 4. Translation

- Kernel-owned devices share one identity-mapped domain covering `[0, top)`,
  where `top` is one past the highest physical frame the memory manager
  serves, with read and write permission. Enabling translation changes no
  kernel device's behavior: the identity domain covers every address the
  memory manager can have handed a kernel driver.
- A passthrough context entry is never written, on any unit: every device
  translates through a real page-table walk.
- A userland driver's device gets its own domain. Its device addresses are
  allocated above the top of physical memory and are never valid physical
  addresses. A map call past the domain's address space is refused to the
  caller.
- Mapping granularity is 2 MiB: a grant is a whole number of 2 MiB pages,
  and there are no smaller grants.
- The unit never observes a stale translation: table writes reach it before
  any dependent device action, and invalidations are issued on map and unmap
  alike — identically on every unit, whatever the unit's capabilities state.
  A unit that does not acknowledge an enable or an invalidation within the
  fixed bound is a panic: a half-enabled unit has an unstatable reach.

## 5. Interrupt remapping

- Compatibility-format interrupt messages are blocked machine-wide.
- Every remappable interrupt names an entry in the kernel's remap table, and
  the unit verifies the requester: a device can fire only entries that name
  that device. A full remap table refuses the device claim that needed the
  entry.
- A device can still fire its own entries at will. Past a fixed per-boot
  ceiling the kernel disables the entry and kills the owning process.

## 6. Userspace handoff refusals

A device is handed to a userland driver only when the unit can isolate it.
Both conditions are computed at enumeration from firmware's declared topology
and access-control capabilities, and a refusal names its reason:

- **Shared isolation scope.** A device whose traffic can reach or be reached
  by another device without the unit seeing it is refused, naming the other
  scope member, and stays a kernel device.
- **Firmware-reserved region.** A device that firmware requires to keep an
  identity-mapped region is refused: honouring the region would grant the
  driver memory it was never given. Kernel-owned devices satisfy such
  regions through the identity domain.

## 7. Reclaim

When a driver process dies, in order:

1. Mask the device's interrupt entries and clear its bus-mastering enable.
   This step is taken at the point of death, before anything else.
2. Reset the function where the device supports reset; otherwise continue.
3. Unmap every range in the domain.
4. Invalidate, and wait for the unit's acknowledgement (§2.3's bound).
5. Return the pages to the allocator — only now.
6. Destroy the domain. The device class is claimable again.

Reclaim never waits on the device's cooperation: a device that ignores its
reset still loses bus mastering in step 1 and its translations in steps 3–4.

## 8. Faults

A device access the tables do not permit is blocked by the unit and recorded.
The kernel decodes each record and logs one line naming the requester, the
address, the access type and the reason; the raw reason code is always
printed.

- A fault on a **kernel-owned** device halts every CPU with the record on
  screen and serial, in §3's form.
- A fault on a **userland driver's** device clears the device's bus-mastering
  enable, logs the record, and kills the owning process. The kernel
  continues, and the device class is claimable again.

A device that appears on the bus after boot has no context entry; its first
DMA faults and is handled as above.

## 9. Exclusions

- **Device-side TLBs.** Teardown must never depend on the device answering.
- **Shared virtual memory, PASID, nested translation, interrupt posting.**
- **First-level translation.** A device address space is never a process's
  page tables.
- **5-level device page tables and 4 KiB device mappings.**
- **A no-IOMMU fallback** (§3).
- **PCIe hotplug.** §8's fault path is the whole answer to a new device.
- **Device-address randomization.** A domain is exclusively its owner's.
- **Non-Intel backends.** Only VT-d is implemented; AMD-Vi and SMMU machines
  refuse at boot (§3).
