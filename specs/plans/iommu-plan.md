# The IOMMU stages that are left

`specs/iommu-spec.md` is the subsystem — what the unit is for, the portable
seam, discovery, the DMA and interrupt-remapping rules, faults, scopes and
reclaim. This file is the order the rest of it gets built in, and it dies when
I5 lands.

## Where this stands

Discovery and translation are built, and what they do is stated as law in the
spec rather than re-litigated here. `kernel/src/iommu/` inventories the
machine's units, gives every function `pci::enumerate` returned a context entry
naming one identity-mapped domain, arms the unit's fault event and turns
translation on. Nothing is refused: a unit this kernel cannot program is left
switched off with a line naming the register it decided on, which leaves that
machine exactly as it boots with no unit at all.

Three gates stand on it. `iommu_discovery` boots four machines differing only
in the unit and asserts the guest's decode moves with them. `iommu_context_absent`
and `iommu_empty_domain` strand one function above and below its context entry
and assert the boot dies with a DMA fault naming that function — the only two
things in the suite that can tell a translating unit from one that is merely
switched on, because under identity mapping every device that *is* in the
tables behaves the same either way.

What is not built is everything the subsystem exists for: interrupt remapping,
per-driver domains with the map/unmap/invalidate path, and the refusal.

## The stages

Every stage leaves the tree green: `cargo run -- --build-only` clean, `cargo
test` green including gate A's fast tier. Exit criteria are runnable commands,
not descriptions.

| Stage | Content | Exit criterion |
|---|---|---|
| **I3** | **Interrupt remapping on.** Remappable IOAPIC redirection entries and a remappable `pci::enable_msi`/`enable_msix`; `IRTA`, `SIRTP`, `IRE=1`, `CFI=0`; every IRTE `SVT`-verified. Spec §6. | `cargo test -- metal_sim_input xhci audio nvme esp` green — every one of these depends on an interrupt arriving. Teeth: an IRTE written with the wrong source-id must red the corresponding test |
| **I4** | **Domains, mapping, invalidation, faults.** `create_domain`/`attach`/`map`/`unmap`/`flush`, the `Unmapped`/`Flushed` pair, the IOVA allocator, and the half of the fault handler that kills a process instead of halting the machine. No syscall yet; driven by an actuator self-test. Spec §5.3, §5.5, §5.6, §7.1, §7.5. | `cargo test -- iommu_selftest`, which maps, reads back through a device, unmaps, and asserts a stale access faults. Teeth: deleting the unmap-side invalidation must red it |
| **I5** | **The refusal.** Every observation the spec's §2.2 table names becomes a halt with the reason on screen instead of a line and a boot that continues. | `cargo test -- iommu_refusal` — three variants (no `intel-iommu`; `intremap=off`; a unit whose `SAGAW` the kernel rejects, via actuator), each asserting its own message and that userland is never reached |

### I3 — the step that can break the boot

Enabling `IRE` changes how *every* interrupt message on the machine is
interpreted, so every source the kernel has already programmed has to be
reprogrammed into remappable format first: the I/O APIC redirection entries the
i8042's two lines use, and every kernel MSI/MSI-X — xHCI, virtio-net,
virtio-sound and HDA all reach the bus through `pci::enable_msi`/`enable_msix`,
which writes the compatibility-format `0xFEE0_0000` that `CFI=0` blocks.

Get it wrong and the machine boots to a black screen with no interrupts, which
is the hardest failure mode there is to bisect. It lands alone, against a suite
where every input, storage and audio test already depends on an interrupt.

Two things to verify on the first `intremap=on` boot rather than assume: that
QEMU blocks compatibility-format messages once `CFI` is clear, and that the
unit's own fault-event MSI is exempt from that block. A fault-reporting channel
that stops working the moment faults become possible is the worst version of
this bug, and the unit is armed before `TE` today precisely so that it works.

### I4 — what the fault handler still owes

The handler decodes each record, logs one line naming the stream, address,
access and reason, and halts every CPU. That is the whole of the spec's §7.1
for as long as every stream on the machine is kernel-owned. I4 is where the
other half arrives, and it is three separate pieces:

- clearing Bus Master Enable on the offending function, which is what lets the
  handler stay bounded once it no longer stops the machine;
- the first-fault latch, the counter, the per-domain flag and the
  `need_resched` handoff that turn a fault into a killed process;
- the storm ceiling of §6.4, which needs an IRTE to clear and a process to kill.

The portable seam grows here too. `DomainId`, `DmaPerm`, `IommuError` and
`trait Iommu` are sketched in the spec's §3 and are deliberately not in the
tree: with one domain and one backend each would have a single value or a
single implementor. I4 is the stage that gives them more than one.

I4's teardown assumes `process::exit` gains an explicit reclaim phase (spec
§7.5). If `specs/assessments/capability-handles-spec.md` stage B2 has landed,
`DeviceClaim`'s `on_zero_handles` is where step 1 of that ordering lives and
exit runs the rest; if it has not, `device.rs`'s existing release path is.
Neither ordering blocks the other. What is not negotiable is that the slow half
never runs from the deferred zero-handle queue.

### I5 — the refusal, and why it is last

The refusal is sequenced after `specs/plans/userspace-drivers-spec.md`'s first
driver has moved, because before that it costs every machine that has no
vIOMMU — which is the default for QEMU, VirtualBox, VMware, Parallels and
essentially every cloud instance type — and protects nothing that has moved.

Two refusal rules need restating before they can be written, and both would
refuse the harness's own machines as the spec words them today:

- **`ECAP.EIM`.** The rule is a refusal "on a machine using x2APIC ids above
  255", and this kernel enables x2APIC while QEMU's unit reports `EIM` clear.
  The rule has to be stated in terms of the ids actually in use, not of x2APIC
  being on.
- **Isolation scopes (spec §7.3).** The rule refuses a device whose scope is
  not a singleton, and it was written for peer-to-peer behind a switch. A
  root-complex-integrated function is not that, and on the T14 both the audio
  and the networking targets are functions of a multi-function device.
  `specs/plans/hda-driver-plan.md` H0 reads the scope off the real machine;
  restating the rule is the spec's decision and the owner's call.

## Ordering

The stages run in their numbered order, and all three are independent of the
capability-handles migration. I3 lands alone because of what it can break; I4
is what the first userspace driver needs and I3 is what makes handing it one
safe, so neither is skippable before I5.
