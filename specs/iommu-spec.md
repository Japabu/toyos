# ToyOS IOMMU

The kernel subsystem that gives a device its own address space, its own interrupt
namespace, and no way out of either. It exists so that a device driver can be an
ordinary userland process: `specs/plans/userspace-drivers-spec.md` is the plan
that spends it, and `specs/plans/iommu-plan.md` is the order the rest of what is
written here gets built in.

`kernel/src/iommu/` inventories the machine's units, gives every enumerated PCI
function a context entry naming one identity-mapped domain, arms the unit's
fault event and turns translation on. Interrupt remapping (§6), per-driver
domains and the map/unmap path (§5.3, §5.5, §5.6) and the refusal (§2) are
stated here and not yet written.

Until they are, a device address a kernel driver hands out is a physical
address — `KernelSlice::phys()`, which is `DirectMap::phys_of` — and the
identity domain is what makes that subtraction still the whole of the
translation. `Iova::identity` is the one place that policy is stated (§5.7).

---

## 1. Purpose

Three properties, in the order they matter:

1. **A device can reach only the memory its driver was given.** A NIC whose driver
   is a userland process must not be able to write the kernel's page tables, another
   process's heap, or the boot stick's cache pages.
2. **A device can raise only the interrupts its driver was given.** This is not a
   follow-up to (1) and cannot be deferred behind it. A device generates an MSI by
   *writing memory* — the address `0xFEE00000`, which VT-d decodes as an interrupt
   request rather than as a translated memory access. DMA remapping does not cover
   that range at all. Without interrupt remapping, a driver process with a mapped
   BAR can inject an arbitrary vector on an arbitrary CPU, which is a kernel-code-
   execution primitive.
3. **A page released by a driver is not reachable by its device.** The classic IOMMU
   use-after-free: unmap without invalidating the IOTLB and the device keeps a
   translation for a page the PMM has already handed to somebody else. Same shape as
   `SYS_PIPE_MAP`'s mapping outliving its page and `FileBacking` outliving unlink
   (`specs/issues/isolation/`), with a DMA engine instead of a process on the
   reading end.

It is not a performance or virtualization feature: enabling it costs a
page-table walk per untranslated device access, and it is bought entirely by
(1)–(3).

---

## 2. No usable IOMMU means ToyOS does not boot

Not "userspace drivers are disabled" — the system refuses. A fallback path
that runs the same drivers with no protection is a second configuration nobody
tests, and its existence would make every security claim above conditional on
a boot flag.

The refusal is the last stage of `specs/plans/iommu-plan.md`, and until it lands
every case below is reported as a line naming the register the kernel decided
on, with the boot continuing and the unit left switched off — which leaves that
machine exactly as it boots with no unit at all.

### 2.1 Where it is detected

`iommu::init` runs in the storage boot phase, after ACPI is readable and
`pci::enumerate` has returned, before any driver `init` (`kernel/src/main.rs`).
The unit must be programmed before the first device is told to do DMA, and every
function the walk returned must have a context entry before translation comes on.

### 2.2 What is distinguishable, and what is not

This matters because a user can fix one of these in firmware setup and not the
other. ACPI alone cannot separate the first two cases, and the message says so
rather than guessing.

| Observation | Meaning | Distinguishable? | Message |
|---|---|---|---|
| No `DMAR` table in the XSDT | Either the platform has no VT-d silicon, **or** firmware has VT-d disabled and therefore publishes no table | **No.** Firmware omits the table in both cases. Probing a hardcoded MCHBAR-relative register window to tell them apart is exactly the model-table legacy guessing this project bans | `iommu: no DMAR table — this platform has no IOMMU, or VT-d is disabled in firmware setup (look for "VT-d" / "Intel Virtualization Technology for Directed I/O"). ToyOS requires one.` |
| `DMAR` present, DRHD register window reads all-ones or `VER_REG` major nibble is 0 | The unit is described but not decoded — firmware bug, or a unit left powered down | **Yes** | `iommu: DRHD at 0x…: register window reads 0x…, the unit is described but not present` |
| `DMAR` present, flags bit 0 (`INTR_REMAP`) clear | The platform declares it cannot remap interrupts | **Yes** | `iommu: platform does not support interrupt remapping (DMAR flags=0x…)` |
| `DMAR` present, unit healthy, `ECAP.IR` clear | This unit cannot remap interrupts | **Yes** | `iommu: unit at 0x… has no interrupt remapping (ECAP=0x…)` |
| `DMAR` present, unit healthy, `CAP.SAGAW` offers no depth we implement | The unit's page-table depths are all unsupported | **Yes** | `iommu: unit at 0x… supports no address width we implement (CAP=0x…)` |
| `DMAR` present, unit healthy, `CAP.SPS` bit 0 clear | No 2 MiB superpage support; see §5.4 | **Yes** | `iommu: unit at 0x… cannot map 2 MiB pages (CAP=0x…)` |

A malformed `DMAR` — a structure or a device scope declaring a length its parent
cannot hold — ends the walk at that element and is reported with the offset and
the length it claimed, never a panic and never a resynchronised walk over
whatever follows. Firmware bytes are untrusted input.

The rest of §4.2's table is the same kind of observation and reaches the same
place: a unit with no `ECAP.QI`, one that cannot name a domain, or one whose
fault recording registers do not fit its own 4 KiB register window.

Every refusal prints the raw register value it decided on. A refusal that says only
"unsupported" is a refusal nobody can act on, and this is the one message that will
be read off a laptop panel with no serial port.

The refusal is a **halt with the reason on screen**, not a panic: it takes the same
`panic_console::boot_checkpoint` path the six boot phases use, so on a machine with
no console the reason is on the panel. It is not a kernel bug and must not print a
backtrace over its own explanation.

### 2.3 What this excludes

- **Every AMD machine.** AMD-Vi is a different unit with a different table (`IVRS`).
  The seam in §3 admits an AMD-Vi backend; this spec does not write one, so an AMD
  box refuses until somebody does.
- **Every Intel machine with VT-d off in firmware setup** — fixable by the user,
  which is why the message names the setting.
- **Intel parts without VT-d.** Historically most Atom/Celeron/Pentium-branded
  parts. The 2020+ Core targets this project names all have it.
- **Every virtual machine whose hypervisor exposes no vIOMMU** — which is the
  default for QEMU, VirtualBox, VMware, Parallels, and essentially every cloud
  instance type. **This is the design's largest cost, and it is a
  development-ergonomics cost, not a security one:** on the day the refusal lands,
  a `cargo run` without `-device intel-iommu` stops booting. That is why the
  refusal is sequenced last, and why every profile in the harness and in
  `src/qemu.rs` already carries the unit.
- **ARM64**, until an SMMU backend exists. Same seam, no code here.

---

## 3. The portable seam

CLAUDE.md requires portability; the concrete requirement is that **Intel's register
layout must not leak into the type names.** No `Vtd`, no `Dmar`, no `Sagaw`, no
`SourceId` above `kernel/src/iommu/vtd/`. Everything in `kernel/src/iommu/mod.rs`
is stated in terms an SMMU also answers.

```rust
/// An address a *device* uses. Never a physical address, never a virtual one.
/// Distinct from PhysAddr because confusing them is the whole bug class.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(transparent)]
pub struct Iova(u64);

/// The unit's name for whoever issued a request: VT-d's 16-bit source-id,
/// an SMMU StreamID. Constructed only at the one site that binds a device.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StreamId(u32);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DomainId(core::num::NonZeroU16);

#[derive(Clone, Copy)]
pub struct DmaPerm { pub read: bool, pub write: bool }

#[derive(Clone, Copy, Debug)]
pub enum IommuError {
    NoUnitForStream(StreamId),
    Unsupported(Capability),
    IovaExhausted,
    RangeNotResident,      // the caller's VA range has no physical page behind it
    AlreadyMapped(Iova),
    NotMapped(Iova),
    StreamBound,           // already attached to a domain
    ScopeShared(StreamId), // shares an isolation scope with another device (§7.3)
    ReservedRegion,        // the device carries a firmware-reserved region (§7.4)
}

pub trait Iommu: Send + Sync {
    fn address_width(&self) -> AddressWidth;

    fn create_domain(&self) -> Result<DomainId, IommuError>;
    fn attach(&self, d: DomainId, s: StreamId) -> Result<(), IommuError>;
    fn detach(&self, d: DomainId, s: StreamId, _: &StreamQuiesced);
    fn destroy_domain(&self, d: DomainId, _: &DomainEmpty);

    fn map(&self, d: DomainId, at: Iova, phys: PhysAddr, len: u64, perm: DmaPerm)
        -> Result<(), IommuError>;

    #[must_use]
    fn unmap(&self, d: DomainId, at: Iova, len: u64) -> Result<Unmapped, IommuError>;

    /// The only constructor of `Flushed`. Synchronous: returns when the unit
    /// has acknowledged that nothing cached the range any more.
    fn flush(&self, d: DomainId, r: IovaRange) -> Flushed;

    /// "Program this into your MSI capability." The *values* are arch-specific;
    /// the shape is not — GICv3's ITS answers the same question.
    fn bind_interrupt(&self, s: StreamId, v: Vector, cpu: CpuTarget)
        -> Result<MsiMessage, IommuError>;
    fn unbind_interrupt(&self, b: InterruptBinding);
}
```

`AddressWidth` is a closed enum rather than a number, so "39" cannot be passed
where a level count is wanted; VT-d's AGAW encoding and an SMMU's `T0SZ` are both
derived from it inside their own backends. It has exactly two variants, `Bits39`
and `Bits48`, because §5.3 takes the widest of those two and §10.5 rules 57-bit
out — a third would be a variant with no producer and no consumer.

**Only the types a caller exists for are written.** `AddressWidth`, `StreamId`
and `Iova` are in the tree; `DomainId`, `DmaPerm`, `IommuError` and `trait Iommu`
are not, because with one domain and one backend each would be a type with a
single value or a single implementor — the dead abstraction §5.2 spends a section
arguing against. They arrive with the stage that gives them a second.

An ARM SMMU backend drops in behind this: a domain is a stage-2 translation table,
`attach` writes a stream-table entry, `flush` issues `CMD_TLBI_S2_IPA` + `CMD_SYNC`,
and `bind_interrupt` returns the ITS doorbell address and an EventID. **No ARM code
is written here and IORT is not parsed** — the requirement is only that the seam
does not have to change to admit it.

---

## 4. Discovery

### 4.1 The DMAR table

`Dmar::open` reads the table through `acpi::find_table`, which walks the XSDT,
bounds the declared length against `MAX_TABLE_LEN` and checksums over exactly
that length. The structure walk is `parse_madt`'s, including its rule that an
entry declaring zero length, or a length that runs past the list holding it,
ends the walk — there is no way to resynchronise a self-describing list that
lied about an element's size. The same rule applies one level down, where a
device scope is bounded by its own structure rather than by the table.

Nothing on this path allocates and nothing is stored: a `Dmar` is a validated
`(base, len)` pair and every structure in it is an offset into the table where
firmware left it.

Fields this kernel reads:

- **Table header + 36:** Host Address Width (`HAW - 1`). Bounds every physical
  address the unit can produce.
- **+37:** Flags. Bit 0 `INTR_REMAP` — its absence is a refusal. Bit 1
  `X2APIC_OPT_OUT` — logged. Bit 2 `DMA_CTRL_PLATFORM_OPT_IN_FLAG` — the platform
  asks that DMA be blocked before the OS enables the unit; honoured by §5.6's
  ordering (nothing is told to DMA before translation is on), so it costs nothing
  and is logged.
- **Remapping structures from +48**, each `(type: u16, length: u16)`:
  - **Type 0 `DRHD`** — flags (bit 0 `INCLUDE_PCI_ALL`), segment, register base
    address, device scopes. One per hardware unit. The `INCLUDE_PCI_ALL` unit is the
    catch-all and the spec requires it last.
  - **Type 1 `RMRR`** — firmware-reserved memory regions, §7.4. Parsed and honoured.
  - **Types 2–6** (`ATSR`, `RHSA`, `ANDD`, `SATC`, `SIDP`) — skipped by length, and
    the skip is logged once with the type so a machine that carries one is not
    silently under-configured. `ATSR` in particular is skipped *because* §10 rejects
    device-TLB.
- **Device scope** entries: type, length, enumeration id, start bus, then a
  `(device, function)` path. Scope type 1 (PCI endpoint), 2 (sub-hierarchy),
  3 (IOAPIC — needed by §6), 4 (MSI-capable HPET), 5 (ACPI namespace device).

**A DRHD's register base is firmware's number and is checked before anything is
mapped at it**: 4 KiB-aligned and under x86-64's 52-bit physical ceiling, never
clamped to fit. A base pointing into usable RAM would read plausible garbage as a
capability register — a wrong log line, and a register write into somebody's heap.

**A device scope whose path runs through a bridge yields no `StreamId`.** The
requester id is not in the table there: it needs each bridge's secondary bus
number out of that bridge's own config space, and guessing would name a different
device. Such a scope is logged with the start bus and the path length instead, so
the line is still a name a bus walk can be matched against.

Register offsets and field positions come from the Intel VT-d architecture
specification's register chapter and structure-format chapter, and **this
document is not the normative source for them.** What makes the decode *checked*
rather than cited is the harness: it stages units differing in `CAP.SAGAW` and in
`ECAP.IR` and asserts the guest's decode moves with them, and a unit programmed
through a wrong offset does not translate at all, which every profile in the
suite depends on.

### 4.2 Unit capabilities that are read, and what each decides

Read once at init, logged once as a single line so a laptop panel carries it, and
then never re-read. `CAP` and `ECAP` are held raw and decoded on demand; nothing
goes back to the register per field.

| Register field | Decides | Refusal if |
|---|---|---|
| `CAP.SAGAW` | page-table depth (§5.3) | no bit we implement |
| `CAP.MGAW` | the widest address the unit accepts; bounds every IOVA (§5.3) | — |
| `CAP.SPS` bit 0 | 2 MiB leaf entries (§5.4) | clear |
| `CAP.CM` | whether a not-present→present transition needs an invalidation (§5.5) | never — both values are supported |
| `CAP.ND` | domain-id width | it cannot name domain 1, which is the one kernel-owned devices are put in (§5.1) |
| `CAP.FRO`, `CAP.NFR` | where the fault recording registers are and how many | they do not fit the unit's own 4 KiB register window |
| `CAP.PSI` | page-selective invalidation; without it every flush is domain-wide | never — domain-wide is correct, just coarser |
| `ECAP.C` | whether page-table walks snoop the CPU cache (§5.2) | never — see §5.2 |
| `ECAP.QI` | queued invalidation | absent — §10.9, and the unit is left unprogrammed rather than served through a register path nothing tests |
| `ECAP.IR` | interrupt remapping (§6) | clear |
| `ECAP.EIM` | 32-bit APIC ids in IRTEs | see §8.1 — the rule has to name the ids in use, not x2APIC |
| `ECAP.SC` | that a second-level entry may force a device's DMA to snoop the CPU cache whatever the device asked for. Read for the log line and for `specs/plans/hda-driver-plan.md` §4.4 | never |
| `ECAP.PT` | nothing. Logged only — §5.7 writes an identity-mapped domain on every unit, including one that offers passthrough | never |

---

## 5. DMA remapping

### 5.1 Domains, and who gets one

- **One domain per userland driver process, holding exactly one device.** Not one
  domain per device shared between processes, and not one domain per process holding
  several devices — a process that claims two devices gets two domains, so that a
  driver bug in one cannot reach the other's buffers through a shared IOVA space.
- **All kernel-owned devices share one identity-mapped domain** (§5.7). The kernel
  is trusted; giving its own devices translated domains would cost page tables and
  buy nothing, because a kernel driver bug is already a kernel bug. Its domain id
  is **1 and never 0**: zero is what an all-zero context entry names, so using it
  would make "the entry this kernel wrote" and "the entry it did not" the same
  value in a fault record and in a domain-selective invalidation.
- **Every function `pci::enumerate` returned gets a context entry before translation
  is enabled**, naming the identity domain. A function with no context entry faults on
  its first DMA, and enabling translation with an unenumerated device on the bus is
  how a machine bricks its own boot disk. Consequence, stated because nothing
  enforces it: **a device that appears after boot has no context entry and will
  fault.** PCIe hotplug is unimplemented, so this is a documented limit rather than a
  live defect. USB hotplug *is* implemented and is not an instance of it: a USB device
  has no requester id of its own, and the DMA on its behalf is the xHC's, whose entry
  was written at boot.

Root table: 4096 bytes, 256 entries of 16 bytes indexed by bus. Context table per
bus: 256 entries of 16 bytes indexed by `(device << 3) | function`, allocated lazily
per bus that has a device on it. Both, and every second-level table, are 4 KiB
sub-allocations of 2 MiB PMM pages under `Category::Dma`: the PMM's granularity is
2 MiB and a table is 4 KiB, so a page per table would waste 511 of every 512. On
QEMU one 2 MiB page holds the lot — a 48-bit identity domain over 6 GiB of address
space is 3072 leaves in 6 page directories, so root + context + the second-level
root + its next level + 6 page directories + an invalidation queue and its status
page is 12 tables of the 512 a page carves into.

The pages are never handed back. These tables live as long as the machine does,
and releasing one would need an invalidation before it (§5.5) — an ordering
`Unmapped`/`Flushed` expresses and this allocator cannot.

### 5.2 `ECAP.C`, and why this kernel does not branch on it

`ECAP.C` set means the unit's page-table walks are coherent with the CPU cache: a
store to a page-table entry is visible to the unit without any flush. `ECAP.C` clear
means it is not, and every modified cache line of every table — root, context,
second-level at each level, and the interrupt remap table — must be written back
before the hardware may be allowed to see it.

A branch on it is a silent-bug generator in the specific way this project keeps
finding: code written and tested on one value of the bit corrupts translations on
a machine with the other, with no test in reach able to tell.

**Decision: flush unconditionally, and do not read `ECAP.C` for anything except the
log line.** The cost is a cache-line flush per 2 MiB of mapping, which is nothing
next to the mapping itself. The alternative — a branch whose false arm no machine
in reach executes — is precisely the class `specs/assessments/metal-track-history.md`
records.

The flush is `clflush` + `mfence` and the two are inseparable in the type: `Table`
has no write that can be called without its flush. `clflushopt` is not used because
it needs its own CPUID bit and QEMU's `qemu64` does not have one; the fence is
what makes the flush globally visible before the MMIO write that tells the unit to
look. This is not decoration — QEMU's unit reports `ECAP.C` clear, so on the
machines the suite runs an entry left in a dirty line is an entry the unit does not
see.

If a profile ever shows the branch to matter, it can be added *by somebody with a
machine reporting the other value to test it on*. Not before.

### 5.3 `CAP.SAGAW`, page-table depth, and where IOVAs live

`SAGAW` is a bitmask of the adjusted guest address widths the unit supports: bit 1 =
39-bit (3-level), bit 2 = 48-bit (4-level), bit 3 = 57-bit (5-level). The context
entry's `AW` field selects one *per device*, and it must name a width the unit
advertises — writing an unsupported one produces a context-entry-invalid fault on
the device's first transaction, which is a fault that looks like a device problem
and is not.

Policy: **take the widest of 48 and 39 that the unit advertises.** 57-bit is not
used — it is a fifth level of page tables for an address space nothing here needs,
and an unused level is an untested one.

**The IOVA space starts at half the domain's address width and nothing is ever
mapped below it:**

```rust
const fn iova_base(aw: AddressWidth) -> Iova { Iova(1u64 << (aw.bits() - 1)) }
```

48-bit → 128 TiB. 39-bit → 256 GiB. This is one rule, derived from the unit rather
than invented, and it does three things at once:

1. **It puts every IOVA above the top of physical RAM.** The PMM's ceiling is
   `MAX_PAGES` × 2 MiB = 64 GiB (`mm/pmm.rs`), so both bases clear it.
   Asserted at init against the real memory map; a machine that violates it is
   refused by name rather than mapped wrongly.
2. **It puts every IOVA above `0xFEE00000`,** so the interrupt window is not
   allocatable and no explicit hole is needed.
3. **It makes an identity-bypass failure loud instead of silent.** If a device is
   not actually behind the unit — QEMU's virtio devices bypass the vIOMMU unless
   `iommu_platform=on`, which is the exact vacuity trap
   `specs/plans/userspace-drivers-spec.md` §7.2 is built around — then the IOVA the
   driver writes into a descriptor is not a valid physical address and the
   transaction fails visibly. A test that would have passed by accident now cannot.

Property (3) is worth more than it looks: it is the difference between a green gate
that proves translation happened and a green gate that proves nothing.

### 5.4 Leaf size

The kernel is 2 MiB-page-only (`mm::PAGE_2M`, and every map/unmap/translate in
`mm/paging.rs` asserts 2 MiB alignment). Second-level tables therefore use **2 MiB
leaf entries**, which requires `CAP.SPS` bit 0. A unit without it is refused (§2.2)
rather than served by a 4 KiB-leaf path that would be 512× the page-table memory for
the same mapping and dead code on every machine in reach.

Consequence to state: **DMA granularity is 2 MiB.** A driver that wants a 4 KiB
descriptor buffer maps 2 MiB and uses 4 KiB of it, exactly as `DmaPool` does today
(`drivers/mod.rs`). The device can reach the whole 2 MiB. That is a real weakening
relative to a 4 KiB-granular IOMMU and it is inherited from the kernel's page size,
not chosen here.

### 5.5 `CAP.CM` and the invalidation rules

`CAP.CM` set means the unit is permitted to cache **not-present** entries, so making
a translation present requires an invalidation before the device may use it. Clear
means only present entries are cached, and a not-present→present transition needs no
invalidation. QEMU's `caching-mode=on` sets it; most physical Intel units do not.

**Decision: invalidate after every table modification, in both directions, and do
not branch on `CAP.CM`.** Same reasoning as §5.2, plus one more: the `CM=1`
configuration is the *stricter* one and it is the one QEMU can stage, so code that
always invalidates is code the harness can certify. Code that skips the map-side
invalidation would pass on hardware and fail under the test configuration — which is
the right way round, but only if it is never written.

The rules, then:

| Transition | Required |
|---|---|
| not-present → present (`map`) | write PTEs, cache-flush them, then invalidate the range |
| present → not-present (`unmap`) | write PTEs, cache-flush them, then invalidate the range, **then and only then** release the pages |
| permission narrowed | as unmap |
| context entry changed (attach/detach) | context-cache invalidate for the source-id, then a domain-wide IOTLB invalidate, then an IEC invalidate if any IRTE named that source-id |
| domain destroyed | domain-selective IOTLB invalidate before any page returns to the PMM |

Invalidation is **synchronous**: queued-invalidation descriptors followed by an
Invalidation Wait descriptor with a status write, polled to completion. Every IOTLB
descriptor sets drain-reads and drain-writes, because the claim `Flushed` makes is
not "the entry is gone" but "nothing is still using it". The wait is bounded by a
named constant whose expiry is a **panic** — a unit that will not acknowledge an
invalidation has left the machine in a state where the kernel cannot know what a
device can reach, and there is no safe way to continue. This is a kernel/hardware
invariant, not untrusted input; fail-fast applies. A queue error reported in `FSTS`
panics with the same reasoning and names which one, because any of them stalls the
head.

### 5.6 The type that makes "release without invalidating" unrepresentable

The bug is: `unmap` returns, the caller frees the pages, the device still has
a cached translation. The rule is a type rather than a discipline:

```rust
/// Pages whose PTEs are gone and whose translations may still be cached.
/// There is no way to get the pages out except through `release`, and no way
/// to build a `Flushed` except by calling `Iommu::flush`.
#[must_use = "these pages are still reachable by the device"]
pub struct Unmapped { domain: DomainId, range: IovaRange, pages: Vec<PhysPage> }

/// Proof that the unit has acknowledged an invalidation covering `range`.
/// Constructible only by `Iommu::flush`. No Clone, no Default, no pub fields.
pub struct Flushed { domain: DomainId, range: IovaRange, _priv: () }

impl Unmapped {
    pub fn release(self, proof: &Flushed) -> Vec<PhysPage> {
        assert!(proof.domain == self.domain && proof.range.covers(self.range),
                "iommu: release proof does not cover the unmapped range");
        self.pages
    }
}
```

**Releasing with no invalidation at all is unrepresentable** — there is no
other way to obtain the `Vec<PhysPage>`. **Releasing against the wrong range
is checked at runtime**, and the check is a kernel-bug assert, because ranges
are values and no type system here will relate them. Tests cover neither —
they cover the mechanism working end to end (§7 of
`specs/plans/userspace-drivers-spec.md`).

`Unmapped` binds *ordering inside one function*, and it is not a `Drop`
guard — dropping it leaks pages, which is the safe direction, and
`#[must_use]` catches the accident at compile time. The path that matters — a
driver process killed by another CPU — never touches an `Unmapped` on the
victim's stack, because reclaim does not run there (§7.5,
`specs/plans/userspace-drivers-spec.md` §5).

### 5.7 One domain for kernel-owned devices, identity-mapped

Kernel devices see the address space they see today and enabling the unit changes
nothing about the boot storage path. That is what makes turning translation on a
step where **nothing's behaviour changes**, and the full test suite is the evidence.

**One path, always: an identity-mapped second-level domain, and never a
passthrough context entry — including on a unit that advertises `ECAP.PT`.** A
passthrough entry ignores every second-level table this kernel writes, so a
machine that took it would exercise none of the walk the per-driver domains are
built on, and a second arm here is one nothing in the suite forces through. This
is §5.2's rule applied to §5.7, and it costs one page directory per GiB of
address space. `ECAP.PT` is read for the log line and for nothing else.

The domain covers `[0, top)`, where `top` is one past the highest frame the PMM
manages (`pmm::top`). The rule makes "behaviour unchanged" a construction
rather than a hope: every address a kernel driver can hand a device
comes out of the PMM, so that range is exactly what the device could already reach
on a machine with no unit. `top` is taken from the memory manager and not from the
firmware memory map, whose own buffer is ordinary free RAM by the time this runs.

Both leaf permissions are granted, and so are both at every level above the leaf:
the unit ANDs permissions down the walk, so narrowing an interior entry would
narrow every mapping under it, which is not what any caller means.

What it does not buy: a kernel driver bug still scribbles anywhere. The kernel
is the trust domain, so that is not a regression. Isolation begins with
per-driver domains, where an IOVA is *allocated* out of a space above the top
of RAM (§5.3) rather than inherited from a physical address — which is why
`Iova` has one constructor named `identity`: the policy has a single site, and
the stage that stops identity-mapping deletes it and is handed every caller
that assumed it.

---

## 6. Interrupt remapping

### 6.1 Two escape routes, both closed only by remapping

Interrupt remapping cannot be deferred behind DMA remapping:

- **A device writes `0xFEE00000`.** VT-d decodes requests in that range as interrupt
  requests, *not* as memory accesses, so DMA remapping never sees them. A userland
  driver that can point its device at an arbitrary buffer address can therefore
  inject an arbitrary vector.
- **A driver owns its MSI-X table.** The MSI-X table lives inside a BAR — `pci.rs`
  reads its BIR and offset from the capability. ToyOS maps at 2 MiB granularity, so
  carving the table out of a mapped BAR is not expressible. The driver can write any
  address and any data it likes into its own table.

With remapping enabled and compatibility-format interrupts disabled
(`GCMD.IRE = 1`, `GCMD.CFI = 0`), neither route reaches a vector: a
compatibility-format message is blocked outright, and a remappable-format message
names an index into the kernel's interrupt remap table. With `IRTE.SVT` programmed to
verify the source-id, a driver can fire only IRTEs that name **its own device**.

Residual, accepted: a driver can fire its own interrupts at will. That is
a self-DoS on a vector the kernel allocated for it. §6.4 bounds it.

### 6.2 The table

`IRTA_REG` points at the interrupt remap table; its low bits encode the size as
`2^(S+1)` entries of 16 bytes. One IRTE per `(device, vector)` the kernel hands out,
plus one per IOAPIC redirection entry and one per kernel-driven device MSI. A
128-entry table (2 KiB) covers everything this machine has; the constant is a `MAX_`
on the primitive and running out is `ResourceExhausted` to the claiming process, not
a panic.

Every IRTE this kernel writes sets `SVT` to verify the source-id. There is no path
that writes an IRTE with `SVT = 0`; the field is not a parameter of the write
function, so "forgot to set SVT" is unrepresentable rather than reviewed.

### 6.3 The step that can break the boot

Enabling `IRE` changes how *every* interrupt message on the machine is interpreted.
Before `GCMD.IRE` is set, every interrupt source the kernel has already programmed
must be reprogrammed into remappable format:

- **IOAPIC redirection entries.** `ioapic::route` writes a destination and a low
  word today; it grows a remappable form (the RTE's interrupt-format bit, an index,
  and the `SVT`-verified IRTE behind it). The i8042's two PS/2 lines are the live
  consumers.
- **Every kernel MSI/MSI-X.** `pci::enable_msix` and `pci::enable_msi` both write
  `MSG_ADDR = 0xFEE0_0000` — compatibility format, which is exactly what `CFI = 0`
  blocks. Both grow a remappable path, and every kernel driver that raises an
  interrupt goes through one of them: xHCI, virtio-net, virtio-sound and HDA. The
  unit's own fault event is programmed directly into `FEADDR`/`FEDATA` and is the
  exception below.

Get this wrong and the machine boots to a black screen with no interrupts. It is the
highest-risk step in the plan, it lands alone, and its gate is the existing input and
storage tests — every one of which already depends on an interrupt arriving.

Two smaller traps in the same step:

- **Enable order.** Set the root-table pointer, enable queued invalidation, set the
  interrupt-remap-table pointer, then `IRE` with `CFI` clear, and only then `TE`.
  Each `GCMD` write is a one-bit-at-a-time protocol — the register is not read-modify-
  write safe, so a write names every persistent bit that is to stay set, and the
  corresponding `GSTS` bit is polled after each with a bounded wait whose expiry is a
  panic.
- **The unit's own fault-event MSI** (`FEADDR`/`FEDATA`, §7.1) is generated by the
  remapping hardware and not by a device, so it is not subject to `CFI` blocking.
  Expected; **verify on the first `intremap=on` boot** before assuming it, because a
  fault reporting channel that stops working the moment faults become possible is the
  worst version of this bug.

### 6.4 Storms

An interrupt the kernel cannot mask at the source is an interrupt a driver can
generate in a loop. The stub ISR (`specs/plans/userspace-drivers-spec.md` §4.4) counts, and
past a rate ceiling the kernel clears `IRTE.P` for that binding — which the driver
cannot undo, because the IRTE is kernel memory — and kills the process. **The ceiling
check does not log per interrupt.** CLAUDE.md's rule applies with unusual force here:
the log ring is drained by `esp_log` through the boot block device from the idle loop
(`specs/issues/boot-media/`), so an error path that logs per event is an error path that
makes a storm into a storage workload.

---

## 7. Faults, scopes, reserved regions, and reclaim

### 7.1 Fault reporting

Faults are recorded in the fault recording registers at `CAP.FRO`, `CAP.NFR` of them,
and reported by an MSI programmed into `FECTL`/`FEDATA`/`FEADDR`. Polling is not used:
a fault that is noticed a hundred milliseconds later is a fault whose device has been
retrying all along. The event is armed **before** `TE`, so the first transaction a
unit blocks is one it can report; stale records firmware may have left behind are
cleared before the mask comes off.

The handler is bounded and does no allocation, no locking and no logging:

1. Read `FSTS`. If `PPF`, read the fault records from the wrap-around index; if `PFO`
   (overflow), note it in the counter and move on.
2. For each record: extract source-id, fault reason, and the faulting address.
3. **Clear Bus Master Enable on that source-id immediately** — one 32-bit ECAM store
   (`pci.rs`'s config window), bounded, and it stops the device dead. This is the
   whole reason the handler can afford to be simple: the storm ends here.
4. Latch the first record per domain into a fixed slot (first fault wins, the same
   discipline `panic_console::capture` uses), bump a counter, set a per-domain flag,
   and clear the record's F bit and `FSTS`.
5. Set `need_resched`. The scheduler pass reads the flag, logs **one** line, and kills
   the owning process.

The units the handler reads live in a fixed array of atomics, written once during
the boot phase that programs each unit and before its mask comes off — the handler
may take no lock, so a `Lock<Vec<_>>` is not reachable from it.

**A fault on a kernel-owned stream is a kernel bug and stops the machine.** A fault on
a userspace-driver stream is a driver bug and kills that process. That split is
CLAUDE.md's fail-fast/untrusted-input line drawn exactly where it belongs, and it is
why the fault path needs no policy configuration.

**Today every stream on the machine is kernel-owned, so every fault takes the first
half**: the handler decodes each record, logs one line naming the stream, the faulting
address, the access and the reason, and then halts every CPU through
`panic_console::capture` + `apic::halt_all_cpus` — which is what puts the reason on
the panel of a machine with no serial port. Steps 3, 4 and 5, and the counter and
per-domain flag they need, arrive with the stage that has a process to kill: with the
machine about to stop, a device told to stop is indistinguishable from one that has,
and the store would be a line no configuration in reach can show doing anything.

Fault reasons worth naming in the log rather than printing as a number: the
root/context-entry-not-present cases, address-beyond-MGAW, the read- and
write-permission cases, and from the interrupt-remapping band the
compatibility-format-blocked and source-id-verification-failure cases. The raw code is
printed either way, so a name is a convenience and never the record. The numeric codes
are not reproduced in this file because a wrong constant in a spec outlives every
review; in the code they are the ones on which the two independent decoders in reach —
Linux's `dma_remap_fault_reasons` and QEMU's `VTD_FR_*` — agree, and where those
disagree (`0x0c`, `0x0d`) the kernel prints the number and no name. Two are not cited
but observed: `iommu_context_absent` asserts the unit's own record decodes to
`context-entry-not-present`, and `iommu_empty_domain` asserts a reason a *second-level
walk* decides rather than naming one code, because which of them a unit gives for an
all-zero entry is an implementation's choice.

### 7.2 Mapping a source-id back to a process

The handler runs in interrupt context and cannot take a lock. A fixed-size array of
`AtomicU64` — `(source_id, domain_id, generation)` packed — one entry per bound
userspace device, scanned linearly. There are single digits of these. No allocation,
no lock, no map.

### 7.3 Isolation scopes

A device is only isolatable if nothing else can reach its traffic without the unit
seeing it. Two ways that fails on real hardware:

- **Peer-to-peer behind a switch.** Two functions under a PCIe switch that does not
  implement Access Control Services can transact with each other directly; the
  upstream unit never sees it. Linux's `iommu_group` machinery exists for this.
- **Multi-function devices** whose functions are not required to be independently
  isolatable.

Rule: **a device is handed to userspace only if its isolation scope is a singleton.**
The scope is computed at enumeration from the DMAR device scopes, the bridge topology
`pci::enumerate` already walks, and the ACS capability where present. A device whose
scope has more than one member is refused with `ScopeShared`, naming the other
member, and stays a kernel device.

**QEMU's q35 topology is flat and this rule is largely untestable there.** A
`pcie-root-port` with two functions behind it can stage the shape; whether
real ACS enforcement behaves as modelled is a hardware property and the T14 is
the first machine that can answer it. Recorded as an open risk (§11) rather
than as a tested property.

A root-complex-integrated function is not peer-to-peer behind a switch, and
both the T14's audio and networking targets are functions of a multi-function
device; whether the singleton rule refuses them is decided from the real
scope, which `specs/plans/hda-driver-plan.md` H0 reads off that machine.

### 7.4 Reserved regions (`RMRR`)

Firmware may require certain physical ranges to remain identity-mapped for a device —
classically the USB controller (for SMM legacy keyboard emulation) and integrated
graphics. Enabling translation without honouring them breaks the device, silently and
after boot.

Rules:
- A kernel-owned device is in the identity domain, so its RMRRs are satisfied for
  free. This covers the USB and graphics cases that motivate RMRRs in the first place.
- A device with an RMRR is **refused for userspace handoff** (`ReservedRegion`).
  Identity-mapping firmware's range into an untrusted driver's domain would hand that
  driver a window into memory it was never given, and the alternative — ignoring the
  RMRR — breaks the device.
- A reserved region binds only the devices its own scope names. Another RMRR on the
  same machine is not this device's problem.

QEMU publishes no RMRR, so this path is untestable in the harness and its first real
exercise is the T14. Say so at the call site.

### 7.5 Reclaim, and where it runs

The safety-critical ordering, when a driver process dies:

```
1. mask the interrupt (clear IRTE.P) and clear Bus Master Enable
2. (if the function supports FLR) issue Function Level Reset and wait
3. unmap every range in the domain            -> Unmapped
4. flush: invalidate IOTLB + context cache, wait for acknowledgement -> Flushed
5. Unmapped::release(&Flushed)  -> pages return to the PMM
6. detach the stream, destroy the domain
```

Step 1 is **bounded** — two MMIO stores — and is the step that must happen
immediately. Step 2's wait is up to 100 ms by the PCIe specification. Steps 3–4 walk
page tables and poll a hardware queue.

**Therefore reclaim does not run from the deferred zero-handle queue, and does not run
from the idle loop.** `specs/assessments/capability-handles-spec.md` §5.2 drains that
queue at syscall exit, at `do_schedule` entry, and in the idle loop — and putting an
unbounded, uninterruptible device operation in front of `pass()` is precisely the
`esp_log` defect (`specs/issues/boot-media/`). Reclaim runs as an **explicit phase of
`process::exit`**, on the exiting or killing thread's own stack, which is a live
thread context that may block. The zero-handle hook does step 1 only, and enqueues
nothing slow.

Nothing here is a `Drop` guard on the victim's
stack; the kernel does not unwind and a killed thread's frames are discarded without
running destructors. Teardown is driven by the process's death path because that path
is code that runs, on a CPU that is alive.

---

## 8. What QEMU can and cannot certify

Every profile that carries a unit gives QEMU
`-device intel-iommu,intremap=…,caching-mode=on,aw-bits=…` on
`-machine q35,kernel-irqchip=split`; `tests/common/qemu.rs`'s `Iommu` is the machine
dimension and `src/qemu.rs` carries the same configuration for `cargo run`.
`caching-mode` is deliberately not a dimension: it is the stricter configuration, it
is the only one QEMU can stage, and §5.5 refuses to branch on it.

**The unit must be the first `-device` in the argv.** QEMU hands a PCI function the
bypassing address space when the function is created before the unit exists, so a
unit emitted after the devices it is meant to decode is a unit that decodes nothing —
and every guest-side assertion would still pass. `iommu_discovery` checks the
position, not only the presence.

**Certifiable here:**

- DMAR parse, unit discovery, capability decode, and the refusal messages — including
  a real difference between `intremap=on` and `intremap=off`, which exercises the
  distinguishing logic in §2.2 rather than just the absent case.
- Root/context tables, second-level tables, identity-mapped domains for kernel
  devices, and the proposition that translation-on changes nothing (the whole suite
  is the gate).
- Invalidation correctness on both the map and unmap sides, because `caching-mode=on`
  makes the map-side invalidation load-bearing and QEMU's IOTLB is a real cache.
- Interrupt remapping, `SVT` source verification, and the compatibility-format block.
- Faults, fault reporting, and the kill-on-fault path.
- Both address widths (`aw-bits=39` and `48`), which is the only way the §5.3 base
  formula gets exercised at more than one value.

**Not certifiable under QEMU:**

- **A unit whose page-table walks snoop the CPU cache.** QEMU's does not. §5.2's
  unconditional flush exists because a branch here cannot be tested on both arms,
  not because flushing is free.
- **RMRR handling** (§7.4). QEMU publishes none.
- **Real ACS / peer-to-peer isolation** (§7.3). The topology can be staged; the
  enforcement cannot.
- **A physical unit's `CAP.CM = 0` behaviour.** Testing only with `caching-mode=on`
  means the configuration that most real hardware presents is the untested one — which
  is safe only because §5.5 refuses to branch on it.
- **Whether a real device recovers from a mid-DMA FLR.** QEMU's virtio FLR is a model
  reset.
- **Whether the harness's virtio devices are behind the unit at all** (§11).
- **Anything about cost.** TCG's distortion is non-uniform, so the 2× production bar
  is answerable only on the T14 or under KVM, in a same-session A/B.
- **The T14's own DMAR** — how many units, which devices are in scope, whether the
  integrated graphics has its own unit, whether any device carries an RMRR. Unknown
  until a real boot, and the diagnostic boot's one-screen log is where the answer
  will be read.

Two things to verify on the first `intremap=on` boot rather than assume, both flagged
above: that QEMU blocks compatibility-format interrupts when `CFI` is clear, and that
the unit's own fault-event MSI is exempt from that block.

### 8.1 What the vIOMMU advertises

Four things QEMU's `intel-iommu` says about itself are load-bearing for decisions
above, and each is a property of the emulation rather than of any one boot:

- **`ECAP.C` is clear.** The unit's walks do not snoop the CPU cache, so §5.2's
  unconditional flush is not decoration here — it is what makes a table this kernel
  wrote visible to the unit at all. Note `snoop-control=on` sets `ECAP.SC` (bit 7),
  which is a different bit from `ECAP.C` (bit 0) and does not make the unit coherent.
- **`ECAP.PT` moves between QEMU releases.** §5.7 writes an identity-mapped domain
  and never a passthrough context entry either way, so no decision here depends on
  which value a host reports.
- **`ECAP.EIM` is clear** under `eim=auto`, while this kernel enables x2APIC. §4.2's
  refusal on `EIM` is written as "a machine using x2APIC ids above 255", and as
  worded it would refuse the harness's own machines; the rule has to be restated in
  terms of the ids actually in use before it is written.
- **The DRHD carries no `INCLUDE_PCI_ALL`.** §4.1 calls that unit the catch-all, and
  QEMU publishes none: the DRHD lists every PCI function on the machine as its own
  device scope, plus one for the I/O APIC on a pseudo-bus. §5.1's rule that every
  enumerated function gets a context entry therefore has no catch-all to fall back on
  here, and the scope list is the mapping. `iommu_discovery` asserts the two sets are
  equal, so a future QEMU that switches to `INCLUDE_PCI_ALL` reports itself.

`aw-bits` moves `CAP.SAGAW` and `CAP.MGAW` and the DMAR's own `HAW`; `intremap`
moves `ECAP.IR` and the DMAR's `INTR_REMAP` flag. Those two differences are what
`iommu_discovery` asserts on across four machines, and they are the only reason the
decode is known to be reading registers at all rather than printing constants.

### 8.2 A permission this unit does not fault on

**QEMU does not report a fault for an access its cached translation already
forbids.** Its IOTLB records a translation with the permissions of whichever access
populated the entry, and a later access the entry does not allow is dropped by QEMU's
*memory core* — the transaction never reaches the code that writes a fault record, so
no fault event is raised and nothing appears on the host's stderr either.

Two consequences:

- **A negative control has to fail on a device's first touch of a page**, which is a
  read: a controller's first access to its DMA pool is a descriptor *fetch*, and a
  control built on narrowing a write permission populates the entry read-only for the
  whole 2 MiB leaf and then sends every later write silently nowhere — a boot that
  wedges with no fault rather than a boot that fails. `iommu_empty_domain` is
  therefore "a present context entry over an empty domain": the walk finds an
  all-zero second-level entry and refuses the read.
- **Permission narrowing is unaffected, because §5.5 already forbids the shape that
  would hit this.** Narrowing is "as unmap": write the entries, flush them, then
  invalidate — and an invalidation empties the entry that would have masked the
  fault. A branch that skipped the invalidation would be silently *correct-looking*
  here, which is one more reason §5.5 refuses to have one.

---

## 9. Failure modes

| Failure | Behaviour | Recovery |
|---|---|---|
| No DMAR / no usable unit | Halt with a named reason on serial and on the panel (§2.2) | User enables VT-d in firmware, or the machine is unsupported |
| Malformed DMAR (bad length, bad checksum, cyclic scope) | Refusal with the raw bytes named; never a panic | Same |
| Device DMA to an unmapped IOVA | Transaction blocked by hardware; BME cleared in the fault handler; one log line; owning process killed | init respawns the daemon; a fresh claim succeeds |
| Device DMA fault on a kernel-owned stream | The record is logged with the stream, address and reason; every CPU halts through the panic console. Every stream is one of these today | Fix the kernel |
| A unit lacking a capability this kernel needs | Left unprogrammed, with a line naming the register; the refusal turns each into a halt | The machine boots as it does with no unit at all |
| `GCMD` bit never appears in `GSTS` | Panic — a half-enabled unit has a reach nothing can state | None; hardware or programming fault |
| Device writes `0xFEE00000` | Compatibility-format interrupt blocked; interrupt-remapping fault; as above | As above |
| Driver fires its own IRTE in a loop | Counted, not logged; past the ceiling `IRTE.P` is cleared and the process is killed | As above |
| Driver process killed while its device is mid-DMA | §7.5: BME cleared inline; unmap/flush/release on the exit path | Pages return only after acknowledged invalidation |
| Invalidation not acknowledged within the bound | Panic — the kernel cannot know what the device can reach | None; this is a hardware or programming fault |
| IOVA space exhausted | `IovaExhausted` → `ResourceExhausted` to the caller | Driver unmaps; a driver that leaks is buggy |
| Interrupt remap table full | `ResourceExhausted` at claim time | — |
| More units than `MAX_UNITS` | The rest are not inventoried, and the machine is told so | — |
| PCIe device appears after boot with no context entry | Faults on first DMA, handled as any fault | Documented limit; PCIe hotplug is unimplemented. USB hotplug is not an instance: the DMA is the xHC's |
| Device shares an isolation scope | Refused at claim (`ScopeShared`), stays a kernel device | — |
| Device carries an RMRR | Refused at claim (`ReservedRegion`) | — |

No failure mode requires a scan, a timeout on device cooperation, or trusting a
driver. The only panics are for kernel bugs and for hardware that will not answer.

---

## 10. Explicitly not doing

1. **ATS / device-TLB** (`ECAP.DT`, QEMU's `device-iotlb=on`, per-device `ats=on`).
   Invalidating a device's own TLB requires the device to answer, so a wedged device
   stalls teardown — a trust dependency on the exact component that just failed. The
   DMAR's `ATSR` structures are parsed past and logged.
2. **PASID / scalable mode / SVM / nested translation** (`x-scalable-mode`,
   `x-pasid-mode`, `svm`, `NEST`). Machinery for shared virtual memory and for nested
   virtualization. No caller.
3. **First-level (`x-flts`) translation.** Second-level is what a device address space
   is; first-level exists to share CPU page tables with a device, which would tie a
   device's reach to a process's address space rather than to what it was given.
4. **Interrupt posting** (`CAP.PI`). A virtualization feature.
5. **5-level page tables** (57-bit AGAW). A level nothing needs and nothing tests.
6. **4 KiB leaf entries.** §5.4.
7. **A branch on `ECAP.C` or on `CAP.CM`.** §5.2, §5.5 — untestable arms.
8. **A passthrough context entry, or a branch on `ECAP.PT`.** §5.7: a passthrough
   entry ignores every second-level table this kernel writes, so it exercises none of
   the walk everything above rests on, and a second arm here is one nothing forces
   through. Read for the log line, never for a decision.
9. **Register-based invalidation** (`CCMD_REG`/`IOTLB_REG`). Correct and slower, and
   every unit in reach has `ECAP.QI`, so it would be an untestable arm too. Such a
   unit is left unprogrammed and named, which leaves that machine exactly as it boots
   today.
10. **IORT parsing and any ARM code.** The seam admits an SMMU backend; this file
    writes none.
11. **AMD-Vi.** Same.
12. **A no-IOMMU fallback.** §2.
13. **Per-device IOVA-space randomization.** IOVAs index a domain the owning process
    already exclusively holds; there is nothing to guess. (The same argument
    `specs/assessments/capability-handles-spec.md` §14.9 makes about handle values.)
14. **Reclaim on a dedicated kernel thread.** The kernel has no in-kernel thread
    mechanism: `sched::driver::spawn` refuses a task without an address space, and
    adding one is a kernel addition needing its own discussion
    (`specs/plans/blocking-io-plan.md` B2 is where it would come from). §7.5's
    exit-path phase needs none.

---

## 11. Open risks

- **The `IRE` cutover can black-screen the machine** and its failure mode is
  "nothing works", which is the hardest kind to bisect. Mitigated by landing it alone,
  against a suite where every input, storage and audio test already depends on an
  interrupt.
- **Whether the harness's virtio devices are behind the unit at all is unknown.**
  QEMU hands a virtio device the bypassing address space unless it is created
  with `iommu_platform=on`, and that option requires the guest to negotiate
  `VIRTIO_F_ACCESS_PLATFORM`, which this kernel's virtio drivers do not. Under
  identity mapping the two are indistinguishable, so the whole green suite is evidence
  for neither — which is why both isolation gates run on a profile whose devices are
  ordinary emulated PCI functions. It stops being harmless once an IOVA is not a
  physical address: a bypassing virtio device handed one writes to whatever that
  number happens to be. The `VIRTIO_F_ACCESS_PLATFORM` negotiation is a virtio-driver
  change and belongs with whichever stage first needs a virtio device translated.
- **Isolation scopes (§7.3) are modelled, not measured.** The first real answer is the
  T14, and the answer could be that a device this project wants in userspace is not
  isolatable there.
- **RMRR on the T14 (§7.4)** could refuse a device for userspace that the plan assumed
  would move. Unknown until a real boot.
- **2 MiB DMA granularity (§5.4)** is coarser than every other IOMMU-based design, and
  it is inherited rather than chosen. If it ever becomes the limiting factor, the fix
  is a 4 KiB path in `mm/paging.rs` first, not a special case here.
- **`map_mmio` maps device registers with `CachePolicy::DeferToMtrr`**, which is PAT
  entry 0 — write-back, taking whatever type firmware's MTRRs give the physical
  range. It works because the PCI hole is uncacheable there and the effective type is
  the stronger of the two, so the correctness of every MMIO access in this kernel,
  the unit's own register window included, rests on a mechanism the page tables name
  but do not state. Mapping a BAR into a *user* address space inherits the same
  dependency. `specs/issues/hardware/device-registers-trust-firmware.md` is the entry.
- **Cost is unmeasurable in the harness.** The 2× production-cost bar is
  answerable only on hardware, and the expectation is that a translated DMA
  path is slower than an untranslated one by an unquantified amount.
