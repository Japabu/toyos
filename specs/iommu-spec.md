# ToyOS IOMMU — Technical Specification

The kernel subsystem that gives a device its own address space, its own interrupt
namespace, and no way out of either. It exists so that a device driver can be an
ordinary userland process; `specs/plans/userspace-drivers-spec.md` is the plan that
spends it, and this file is the layer underneath.

Split into its own file because the lifetimes differ. The driver migration is a
finite project with an end condition; this subsystem is permanent, it must exist
in full before the first driver leaves the kernel, and it has a second backend
(ARM SMMU) planned behind the same seam. A reader changing `kernel/src/iommu/`
in 2027 should not have to read a migration plan that finished.

**Stages I0–I2 of §9 are built** (`kernel/src/iommu/`): every profile carries a unit,
the machine's units are inventoried, and translation is *on* — every enumerated PCI
function has a context entry naming one identity-mapped domain, so what a device can
reach is unchanged and the mechanism that decides it is now the kernel's. What is not
built is everything the subsystem exists for: interrupt remapping (I3), per-driver
domains and the map/unmap/invalidate path (I4), and the refusal (I5).

Where a device address still comes from, until I4: every descriptor any driver writes
is a raw `DirectMap::phys_of()` result (`mm/mod.rs:134`), reached through
`KernelSlice::phys()` (`mm/region.rs:24`), and the identity domain is what makes that
subtraction still the whole of the translation. `Iova::identity` is the one place that
policy is stated (§5.7).

---

## 1. What this is for, and what it is not for

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
   execution primitive. **A boundary that does not bind what its name implies is
   worse than no boundary**; "userspace drivers, IOMMU-protected" with (2) missing
   is exactly that sentence.
3. **A page released by a driver is not reachable by its device.** The classic IOMMU
   use-after-free: unmap without invalidating the IOTLB and the device keeps a
   translation for a page the PMM has already handed to somebody else. Same shape as
   `SYS_PIPE_MAP`'s mapping outliving its page and `FileBacking` outliving unlink
   (`specs/issues/isolation/`), with a DMA engine instead of a process on the
   reading end.

What this is **not** for: it is not a performance feature, not a virtualization
feature, and not a way to make the kernel smaller. Enabling it costs a page-table
walk per untranslated device access and buys nothing measurable. It is bought
entirely by (1)–(3).

---

## 2. No usable IOMMU means ToyOS does not boot

Owner's decision. Not "userspace drivers are disabled" — the system refuses. The
reasoning is the zero-legacy principle applied honestly: a fallback path that runs
the same drivers with no protection is a second configuration nobody tests, and its
existence would make every security claim in this file conditional on a boot flag.

### 2.1 Where it is detected

At `kernel/src/main.rs`, in the boot phase that today calls `acpi::find_ecam_base`
(`main.rs:345`) — after ACPI is readable and PCI is enumerable, before any driver
`init`. The unit must be programmed before the first device is told to do DMA.

### 2.2 What is distinguishable, and what is not

This matters because a user can fix one of these in firmware setup and not the
other. The honest answer is that ACPI alone cannot separate the first two cases, and
the message must say so rather than guess.

| Observation | Meaning | Distinguishable? | Message |
|---|---|---|---|
| No `DMAR` table in the XSDT | Either the platform has no VT-d silicon, **or** firmware has VT-d disabled and therefore publishes no table | **No.** Firmware omits the table in both cases. Probing a hardcoded MCHBAR-relative register window to tell them apart is exactly the model-table legacy guessing this project bans | `iommu: no DMAR table — this platform has no IOMMU, or VT-d is disabled in firmware setup (look for "VT-d" / "Intel Virtualization Technology for Directed I/O"). ToyOS requires one.` |
| `DMAR` present, DRHD register window reads all-ones or `VER_REG` major nibble is 0 | The unit is described but not decoded — firmware bug, or a unit left powered down | **Yes** | `iommu: DRHD at 0x…: register window reads 0x…, the unit is described but not present` |
| `DMAR` present, flags bit 0 (`INTR_REMAP`) clear | The platform declares it cannot remap interrupts | **Yes** | `iommu: platform does not support interrupt remapping (DMAR flags=0x…)` |
| `DMAR` present, unit healthy, `ECAP.IR` clear | This unit cannot remap interrupts | **Yes** | `iommu: unit at 0x… has no interrupt remapping (ECAP=0x…)` |
| `DMAR` present, unit healthy, `CAP.SAGAW` offers no depth we implement | The unit's page-table depths are all unsupported | **Yes** | `iommu: unit at 0x… supports no address width we implement (CAP=0x…)` |
| `DMAR` present, unit healthy, `CAP.SPS` bit 0 clear | No 2 MiB superpage support; see §5.4 | **Yes** | `iommu: unit at 0x… cannot map 2 MiB pages (CAP=0x…)` |

Every refusal prints the raw register value it decided on. A refusal that says only
"unsupported" is a refusal nobody can act on, and this is the one message that will
be read off a laptop panel with no serial port.

The refusal is a **halt with the reason on screen**, not a panic: it takes the same
`panic_console::boot_checkpoint` path the six boot phases use, so on a machine with
no console the reason is on the panel. It is not a kernel bug and must not print a
backtrace over its own explanation.

### 2.3 What this excludes, stated plainly

- **Every AMD machine.** AMD-Vi is a different unit with a different table (`IVRS`).
  The seam in §3 admits an AMD-Vi backend; this spec does not write one, so an AMD
  box refuses until somebody does.
- **Every Intel machine with VT-d off in firmware setup** — fixable by the user,
  which is why the message names the setting.
- **Intel parts without VT-d.** Historically most Atom/Celeron/Pentium-branded
  parts. The 2020+ Core targets this project names all have it.
- **Every virtual machine whose hypervisor exposes no vIOMMU** — which is the
  default for QEMU, VirtualBox, VMware, Parallels, and essentially every cloud
  instance type. **This is the largest cost in this document and it is a
  development-ergonomics cost, not a security one:** on the day the refusal lands,
  a `cargo run` without `-device intel-iommu` stops booting. §9 sequences the
  refusal last for exactly this reason, and stage 0 of
  `specs/plans/userspace-drivers-spec.md` puts the flag into every profile long before it
  is load-bearing.
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

`AddressWidth` is a closed enum (`Bits39`, `Bits48`, `Bits57`) rather than a number,
so "39" cannot be passed where a level count is wanted; VT-d's AGAW encoding and an
SMMU's `T0SZ` are both derived from it inside their own backends.

An ARM SMMU backend drops in behind this: a domain is a stage-2 translation table,
`attach` writes a stream-table entry, `flush` issues `CMD_TLBI_S2_IPA` + `CMD_SYNC`,
and `bind_interrupt` returns the ITS doorbell address and an EventID. **No ARM code
is written here and IORT is not parsed** — the requirement is only that the seam
does not have to change to admit it.

---

## 4. Discovery

### 4.1 The DMAR table

`kernel/src/drivers/acpi.rs` already has everything needed and one of it is private.
`find_table(rsdp_addr, signature, needed) -> Result<Table, TableError>`
(`acpi.rs:381`) walks the XSDT, bounds the length against `MAX_TABLE_LEN` and
checksums over exactly the declared length. It is `fn`, not `pub fn`. Make it public
(or add `pub fn find_dmar`) and the parse is a walk of variable-length remapping
structures — **the pattern to copy verbatim is `parse_madt` (`acpi.rs:555-611`)**,
including its rule that an entry declaring zero length, or a length that runs past
the table, ends the walk.

Firmware bytes are untrusted input, and `acpi.rs`'s module doc already says so:
nothing on this path panics for any input. A malformed DMAR is a refusal (§2.2),
never a fault.

Fields this kernel reads:

- **Table header + 36:** Host Address Width (`HAW - 1`). Bounds every physical
  address the unit can produce.
- **+37:** Flags. Bit 0 `INTR_REMAP` — its absence is a refusal. Bit 2
  `DMA_CTRL_PLATFORM_OPT_IN_FLAG` — the platform asks that DMA be blocked before the
  OS enables the unit; honoured by §5.6's ordering (nothing is told to DMA before
  translation is on), so it costs nothing and is logged.
- **Remapping structures from +48**, each `(type: u16, length: u16)`:
  - **Type 0 `DRHD`** — flags (bit 0 `INCLUDE_PCI_ALL`), segment, register base
    address, device scopes. One per hardware unit. The `INCLUDE_PCI_ALL` unit is the
    catch-all and the spec requires it last.
  - **Type 1 `RMRR`** — firmware-reserved memory regions, §7.4. Parsed and honoured.
  - **Types 2–6** (`ATSR`, `RHSA`, `ANDD`, `SATC`, `SIDP`) — skipped by length, and
    the skip is logged once with the type so a machine that carries one is not
    silently under-configured. `ATSR` in particular is skipped *because* §11 rejects
    device-TLB.
- **Device scope** entries: type, length, enumeration id, start bus, then a
  `(device, function)` path. Scope type 1 (PCI endpoint), 2 (sub-hierarchy),
  3 (IOAPIC — needed by §6), 4 (MSI-capable HPET), 5 (ACPI namespace device).

Register offsets and field positions are cited here from the Intel VT-d
architecture specification's register chapter and structure-format chapter;
**re-check each against the PDF at implementation time** — they are stable across
revisions but this document is not the normative source and must not be treated as
one.

### 4.2 Unit capabilities that are read, and what each decides

Read once at init, logged once as a single line so a laptop panel carries it, and
then never re-read.

| Register field | Decides | Refusal if |
|---|---|---|
| `CAP.SAGAW` | page-table depth (§5.3) | no bit we implement |
| `CAP.SPS` bit 0 | 2 MiB leaf entries (§5.4) | clear |
| `CAP.CM` | whether a not-present→present transition needs an invalidation (§5.5) | never — both values are supported |
| `CAP.ND` | domain-id width; caps concurrent domains | fewer than 16 domains |
| `CAP.FRO`, `CAP.NFR` | where the fault recording registers are and how many | — |
| `CAP.PSI` | page-selective invalidation; without it every flush is domain-wide | never — domain-wide is correct, just coarser |
| `ECAP.C` | whether page-table walks snoop the CPU cache (§5.2) | never — see §5.2 |
| `ECAP.QI` | queued invalidation | absent → register-based invalidation via `CCMD_REG`/`IOTLB_REG`, which is correct and slower |
| `ECAP.IR` | interrupt remapping (§6) | clear |
| `ECAP.EIM` | 32-bit APIC ids in IRTEs | clear on a machine using x2APIC ids above 255 |
| `ECAP.PT` | nothing. Logged only — §5.7 writes an identity-mapped domain on every unit, including one that offers passthrough | never |

---

## 5. DMA remapping

### 5.1 Domains, and who gets one

- **One domain per userland driver process, holding exactly one device.** Not one
  domain per device shared between processes, and not one domain per process holding
  several devices — a process that claims two devices gets two domains, so that a
  driver bug in one cannot reach the other's buffers through a shared IOVA space.
- **All kernel-owned devices share one identity-mapped domain** (§5.7, as §8.1
  leaves it — passthrough is not available on any unit in reach). The kernel is
  trusted; giving its own devices translated domains would cost page tables and buy
  nothing, because a kernel driver bug is already a kernel bug.
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
space is 3072 leaves in 6 page directories, so root + context + PML4 + PDPT + 6 PDs +
an invalidation queue and its status page is 12 tables of the 512 a page carves into.

### 5.2 `ECAP.C`, and why this kernel does not branch on it

`ECAP.C` set means the unit's page-table walks are coherent with the CPU cache: a
store to a page-table entry is visible to the unit without any flush. `ECAP.C` clear
means it is not, and every modified cache line of every table — root, context,
second-level at each level, and the interrupt remap table — must be written back
before the hardware may be allowed to see it.

This is a silent-bug generator in the specific way this project keeps finding: code
written on a `C=1` machine works, and the same code corrupts translations on a `C=0`
one, with no test in reach able to tell. QEMU sets `ECAP.C`. The T14 is expected to
as well. So the `C=0` path would be dead code on every machine anybody here can boot.

**Decision: flush unconditionally, and do not read `ECAP.C` for anything except the
log line.** One `clflush` (or `clflushopt` + `sfence`) per modified table cache line,
on every path, always. The cost is a cache-line flush per 2 MiB of mapping, which is
nothing next to the mapping itself. The alternative — a branch whose false arm no
machine in reach executes — is precisely the class `specs/metal-track-history.md`
records seventy instances of.

If a profile ever shows this to matter, the branch can be added *by somebody with a
`C=0` machine to test it on*. Not before.

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
   `MAX_PAGES = 32768` × 2 MiB = 64 GiB (`mm/pmm.rs:144`), so both bases clear it.
   Asserted at init against the real memory map; a machine that violates it is
   refused by name rather than mapped wrongly.
2. **It puts every IOVA above `0xFEE00000`,** so the interrupt window is not
   allocatable and no explicit hole is needed.
3. **It makes an identity-bypass failure loud instead of silent.** If a device is
   not actually behind the unit — QEMU's virtio devices bypass the vIOMMU unless
   `iommu_platform=on`, which is the exact vacuity trap
   `specs/plans/userspace-drivers-spec.md` §8 is built around — then the IOVA the driver
   writes into a descriptor is not a valid physical address and the transaction
   fails visibly. A test that would have passed by accident now cannot.

Property (3) is worth more than it looks: it is the difference between a green gate
that proves translation happened and a green gate that proves nothing.

### 5.4 Leaf size

The kernel is 2 MiB-page-only (`mm/mod.rs:17`, and every map/unmap/translate in
`mm/paging.rs` asserts 2 MiB alignment). Second-level tables therefore use **2 MiB
leaf entries**, which requires `CAP.SPS` bit 0. A unit without it is refused (§2.2)
rather than served by a 4 KiB-leaf path that would be 512× the page-table memory for
the same mapping and dead code on every machine in reach.

Consequence to state: **DMA granularity is 2 MiB.** A driver that wants a 4 KiB
descriptor buffer maps 2 MiB and uses 4 KiB of it, exactly as `DmaPool` does today
(`drivers/mod.rs:25`). The device can reach the whole 2 MiB. That is a real
weakening relative to a 4 KiB-granular IOMMU and it is inherited from the kernel's
page size, not chosen here.

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
Invalidation Wait descriptor with a status write, polled to completion; or, on a unit
without `ECAP.QI`, `CCMD_REG`/`IOTLB_REG` polled on their own completion bits. Both
waits are bounded and the bound is a named constant whose expiry is a **panic** — a
unit that will not acknowledge an invalidation has left the machine in a state where
the kernel cannot know what a device can reach, and there is no safe way to continue.
This is a kernel/hardware invariant, not untrusted input; fail-fast applies.

### 5.6 The type that makes "release without invalidating" unrepresentable

The bug is: `unmap` returns, the caller frees the pages, the device still has a
cached translation. Making it a discipline rule fails the same way every discipline
rule in this tree has failed. Making it a type:

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

Ladder position, stated exactly: **releasing with no invalidation at all is
unrepresentable** (there is no other way to obtain the `Vec<PhysPage>`);
**releasing against the wrong range is checked at runtime** and the check is a
kernel-bug assert, because ranges are values and no type system here will relate
them. Tests cover neither — they cover the mechanism working end to end (§8 of
`specs/plans/userspace-drivers-spec.md`).

**And now the caveat CLAUDE.md requires asking of every safety type: which paths does
this bind, and is the failing one among them?** `Unmapped` binds *ordering inside one
function*, and it is not a `Drop` guard — dropping it leaks pages, which is the safe
direction, and `#[must_use]` catches the accident at compile time. The path that
matters — a driver process killed by another CPU — never touches an `Unmapped` on the
victim's stack, because reclaim does not run there. See §7.5 and
`specs/plans/userspace-drivers-spec.md` §5.

### 5.7 One domain for kernel-owned devices, identity-mapped

Kernel devices see the address space they see today and enabling the unit changes
nothing about the boot storage path. This is the single largest de-risking decision in
the plan: the stage that turns translation on is a stage where **nothing's behaviour
changes**, and the full test suite is the evidence.

This section said *passthrough* (`ECAP.PT`) until I2, and called the identity-mapped
alternative "one extra code path that would be exercised only on such a unit." §8.1
measured `ECAP.PT` clear on the only unit anyone here can boot, which inverts that
sentence: identity is the path every machine in reach runs and passthrough is the arm
nothing executes. **Decision at I2: write an identity-mapped second-level domain
always, and never a passthrough context entry, even on a unit that offers one.** This
is §5.2's rule applied to §5.7 — a branch whose false arm no machine in reach takes is
the defect, not the saving — and it costs one page directory per GiB of address space.
`ECAP.PT` is read for the log line and for nothing else.

The domain covers `[0, top)`, where `top` is one past the highest frame the PMM
manages. One rule, and it is what makes "behaviour unchanged" a construction rather
than a hope: every address a kernel driver can hand a device comes out of the PMM, so
that range is exactly what the device could already reach on a machine with no unit.
`top` is taken from the memory manager and not from the firmware memory map, whose
own buffer is ordinary free RAM by the time this runs.

Honest about what it does not buy: a kernel driver bug still scribbles anywhere. That
is unchanged from today and the kernel is the trust domain, so it is not a
regression. Isolation begins at I4, where an IOVA is *allocated* out of a space above
the top of RAM (§5.3) rather than inherited from a physical address — which is why
`Iova` exists at I2 with one constructor named `identity`: the policy has a single
site, and the stage that stops identity-mapping deletes it and is handed every caller
that assumed it.

---

## 6. Interrupt remapping

### 6.1 Why it is not optional and not later

Two independent escape routes, both closed only by remapping:

- **A device writes `0xFEE00000`.** VT-d decodes requests in that range as interrupt
  requests, *not* as memory accesses, so DMA remapping never sees them. A userland
  driver that can point its device at an arbitrary buffer address can therefore
  inject an arbitrary vector.
- **A driver owns its MSI-X table.** The MSI-X table lives inside a BAR
  (`pci.rs:138-171` reads its BIR and offset from the capability). ToyOS maps at
  2 MiB granularity, so carving the table out of a mapped BAR is not expressible.
  The driver can write any address and any data it likes into its own table.

With remapping enabled and compatibility-format interrupts disabled
(`GCMD.IRE = 1`, `GCMD.CFI = 0`), neither route reaches a vector: a
compatibility-format message is blocked outright, and a remappable-format message
names an index into the kernel's interrupt remap table. With `IRTE.SVT` programmed to
verify the source-id, a driver can fire only IRTEs that name **its own device**.

Residual, accepted and stated: a driver can fire its own interrupts at will. That is
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

- **IOAPIC redirection entries.** `ioapic.rs:282`'s `route()` writes a destination
  and a low word today; it grows a remappable form (the RTE's interrupt-format bit,
  an index, and the `SVT`-verified IRTE behind it). The i8042's IRQ1 and IRQ12
  (`i8042/mod.rs:1110-1130`) are the live consumers.
- **Every kernel MSI/MSI-X.** `pci.rs:138` (`enable_msix`) and `pci.rs:173`
  (`enable_msi`) both write `MSG_ADDR = 0xFEE0_0000` (`pci.rs:36`) — compatibility
  format, which is exactly what `CFI = 0` blocks. Both grow a remappable path. The
  live consumers are xHCI vector 0x21 (`xhci/mod.rs:743`), virtio-net 0x22
  (`virtio_net.rs:129`) and virtio-sound 0x23 (`virtio_sound.rs`), the last two
  open-coding their own MSI-X setup rather than going through `pci.rs`.

Get this wrong and the machine boots to a black screen with no interrupts. It is the
highest-risk step in the plan, it is a stage of its own (§9 stage 3), and its gate is
the existing input and storage tests — every one of which already depends on an
interrupt arriving.

Two smaller traps in the same step:

- **Enable order.** Set the root-table pointer, enable queued invalidation, set the
  interrupt-remap-table pointer, then `IRE` with `CFI` clear, and only then `TE`.
  Each `GCMD` write is a one-bit-at-a-time protocol — the register is not read-modify-
  write safe, and the corresponding `GSTS` bit is polled after each.
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
retrying all along.

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

**A fault on a kernel-owned stream is a kernel bug and stops the machine.** A fault on
a userspace-driver stream is a driver bug and kills that process. That split is
CLAUDE.md's fail-fast/untrusted-input line drawn exactly where it belongs, and it is
why the fault path needs no policy configuration.

**Built at I2, and the split lands entirely on its first half**, because there are no
userspace drivers yet and every stream is kernel-owned. So steps 4 and 5, and the
counter and per-domain flag they need, are I4's; the handler decodes each record, logs
one line naming the stream, the faulting address, the access and the reason, and then
halts every CPU through `panic_console::capture` + `apic::halt_all_cpus` — which is
what puts the reason on the panel of a machine with no serial port. Step 3 is I4's for
the same reason: with the machine about to stop, a device told to stop is
indistinguishable from one that has, and the store would be a line no configuration in
reach can show doing anything.

Fault reasons worth naming in the log rather than printing as a number: the
root/context-entry-not-present cases, address-beyond-MGAW, the read- and
write-permission cases, and from the interrupt-remapping band the
compatibility-format-blocked and source-id-verification-failure cases. The raw code is
printed either way, so a name is a convenience and never the record. The numeric codes
are not reproduced in this file because a wrong constant in a spec outlives every
review; in the code they are the ones on which the two independent decoders in reach —
Linux's `dma_remap_fault_reasons` and QEMU's `VTD_FR_*` — agree, and where those
disagree (`0x0c`, `0x0d`) the kernel prints the number and no name. Two are not cited
but observed: the I2 gates assert the unit's own record decodes to `0x02` and `0x06`.

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
`pci::enumerate` already walks (`pci.rs:250`), and the ACS capability where present.
A device whose scope has more than one member is refused with `ScopeShared`, naming
the other member, and stays a kernel device.

Honest about the evidence: **QEMU's q35 topology is flat and this rule is largely
untestable there.** A `pcie-root-port` with two functions behind it can stage the
shape; whether real ACS enforcement behaves as modelled is a hardware property and the
T14 is the first machine that can answer it. Recorded as an open risk rather than as a
tested property.

### 7.4 Reserved regions (`RMRR`)

Firmware may require certain physical ranges to remain identity-mapped for a device —
classically the USB controller (for SMM legacy keyboard emulation) and integrated
graphics. Enabling translation without honouring them breaks the device, silently and
after boot.

Rules:
- A kernel-owned device is in the passthrough domain, so its RMRRs are satisfied for
  free. This covers the USB and graphics cases that motivate RMRRs in the first place.
- A device with an RMRR is **refused for userspace handoff** (`ReservedRegion`).
  Identity-mapping firmware's range into an untrusted driver's domain would hand that
  driver a window into memory it was never given, and the alternative — ignoring the
  RMRR — breaks the device.

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
from the idle loop.** `specs/assessments/capability-handles-spec.md` §5.2 drains that queue at
syscall exit, at `do_schedule` entry, and in the idle loop — and putting an
unbounded, uninterruptible device operation in front of `pass()` is precisely the
`esp_log` defect (`specs/issues/boot-media/`, and the flush that costs 2.0–9.7 ms against
a 23.219 ms audio pipeline). Reclaim runs as an **explicit phase of `process::exit`**,
on the exiting or killing thread's own stack, which is a live thread context that may
block. The zero-handle hook does step 1 only, and enqueues nothing slow.

This also answers the Drop question. Nothing here is a `Drop` guard on the victim's
stack; the kernel does not unwind and a killed thread's frames are discarded without
running destructors. Teardown is driven by the process's death path because that path
is code that runs, on a CPU that is alive.

---

## 8. What QEMU can and cannot certify

Measured on this host, 2026-08-02: `qemu-system-x86_64 --version` reports **QEMU
emulator version 11.0.2**. `qemu-system-x86_64 -device intel-iommu,help` lists
`aw-bits` (default 48), `caching-mode`, `device-iotlb`, `dma-translation`, `eim`,
`fs1gp`, `intremap` (default auto), `snoop-control`, `stale-tm`, `svm`, `version`,
`x-flts`, `x-pasid-mode`, `x-scalable-mode`. All four of
`-device intel-iommu`, `…,intremap=on,caching-mode=on`,
`…,intremap=on,caching-mode=on,aw-bits=48,snoop-control=on` and `…,aw-bits=39`
instantiate cleanly on `-machine q35,kernel-irqchip=split` (QMP `query-status`
returned `prelaunch`, exit 0 for each).

**Certifiable here:**

- DMAR parse, unit discovery, capability decode, and the refusal messages — including
  a real difference between `intremap=on` and `intremap=off`, which exercises the
  distinguishing logic in §2.2 rather than just the absent case.
- Root/context tables, second-level tables, passthrough for kernel devices, and the
  proposition that translation-on changes nothing (the whole suite is the gate).
- Invalidation correctness on both the map and unmap sides, because `caching-mode=on`
  makes the map-side invalidation load-bearing and QEMU's IOTLB is a real cache.
- Interrupt remapping, `SVT` source verification, and the compatibility-format block.
- Faults, fault reporting, and the kill-on-fault path.
- Both address widths (`aw-bits=39` and `48`), which is the only way the §5.3 base
  formula gets exercised at more than one value.

**Not certifiable here, and the list is the honest part of this document:**

- **`ECAP.C = 0`.** QEMU is coherent. §5.2's unconditional flush exists because this
  path cannot be tested, not because flushing is free.
- **RMRR handling** (§7.4). QEMU publishes none.
- **Real ACS / peer-to-peer isolation** (§7.3). The topology can be staged; the
  enforcement cannot.
- **A physical unit's `CAP.CM = 0` behaviour.** Testing only with `caching-mode=on`
  means the configuration that most real hardware presents is the untested one — which
  is safe only because §5.5 refuses to branch on it.
- **Whether a real device recovers from a mid-DMA FLR.** QEMU's virtio FLR is a model
  reset.
- **Anything about cost.** TCG's distortion is non-uniform (CLAUDE.md records 1.06×–
  6.5× by operation), so the 2× production bar is answerable only on the T14 or under
  KVM, in a same-session A/B.
- **The T14's own DMAR** — how many units, which devices are in scope, whether the
  integrated graphics has its own unit, whether any device carries an RMRR. Unknown
  until a real boot, and the diagnostic boot's one-screen log is where the answer
  will be read.

Two things to verify on the first `intremap=on` boot rather than assume, both flagged
above: that QEMU blocks compatibility-format interrupts when `CFI` is clear, and that
the unit's own fault-event MSI is exempt from that block.

### 8.1 What the unit actually says, measured at I1

Read by the kernel on 2026-08-03 under
`-device intel-iommu,intremap=on,caching-mode=on,aw-bits=48` on
`-machine q35,kernel-irqchip=split`, QEMU 11.0.2, and printed by
`cargo test -- iommu_discovery`:

```
DMAR haw=48 flags=0x01 intr_remap=y x2apic_opt_out=n dma_ctrl_opt_in=n
unit0 @0xfed90000 seg=0 pci_all=n ver=1.0
      cap=0x80d2008c222f06c6 ecap=0x0000000000f00f0a
      aw=48 sagaw=0x06 mgaw=48 nd=65536 sps2m=y cm=y psi=y nfr=1 fro=0x220
      qi=y ir=y eim=n pt=n coherent=n
```

Four of those contradict what this file assumed, and each changes a decision a
later stage was going to make on the strength of it:

- **`ECAP.C` is clear.** §5.2 says "QEMU sets `ECAP.C`" and builds its argument on
  the `C=0` path being dead code on every machine in reach. It is the opposite:
  every machine in reach is `C=0`, and the *untestable* arm is `C=1`. The decision
  §5.2 reaches — flush unconditionally, never branch — is unchanged and now has a
  stronger reason. Note `snoop-control=on` sets `ECAP.SC` (bit 7), which is a
  different bit from `ECAP.C` (bit 0) and does not make the unit coherent.
- **`ECAP.PT` is clear.** §5.7 makes passthrough for kernel-owned devices "the
  single largest de-risking decision in the plan". It is not available here, so the
  fallback §5.7 calls "one extra code path that would be exercised only on such a
  unit" is the *only* path the harness can run: at I2 every kernel device gets an
  identity-mapped translated domain, on every profile.
- **`ECAP.EIM` is clear** with `eim=auto`. §4.2 makes that a refusal "on a machine
  using x2APIC ids above 255", and this kernel enables x2APIC. Whether `eim=on`
  changes it is unmeasured; if it does not, I5's refusal would refuse the harness's
  own machines and the rule needs restating in terms of the ids actually in use.
- **The DRHD carries no `INCLUDE_PCI_ALL`.** §4.1 calls that unit "the catch-all",
  and QEMU publishes none: the DRHD lists every PCI function on the machine as its
  own device scope, plus one for the I/O APIC on pseudo-bus `0xff`. §5.1's rule that
  every enumerated function gets a context entry therefore has no catch-all to fall
  back on here, and the scope list is the mapping. Verified against the raw 120-byte
  table rather than inferred.

`aw-bits=39` moves `sagaw` to `0x02`, `mgaw` to 39 and the DMAR's own `haw` to 39;
`intremap=off` moves `ecap` to `0x0000000000000f02` and clears the DMAR flag. Those
two differences are what `iommu_discovery` asserts on, and they are the only reason
the decode is known to be reading registers at all.

### 8.2 A permission this unit will not fault on, measured at I2

**QEMU does not report a fault for an access its cached translation already
forbids.** Its IOTLB records a translation with the permissions of whichever access
populated the entry, and a later access the entry does not allow is dropped by QEMU's
*memory core* — the transaction never reaches the code that writes a fault record, so
no fault event is raised and nothing appears on the host's stderr either.

Measured, and it cost a stage's worth of debugging. I2's second negative control was
first built as "grant the identity domain no write permission, and assert a device's
write is blocked". The result was a boot that wedged in `nvme::init` with no fault, no
kernel output at all, and a ten-second harness timeout: the controller's first access
to its DMA pool is a descriptor *fetch*, which populated the entry read-only for the
whole 2 MiB leaf, and every write it then made to the same page — the completion
queue, the identify buffer — went silently nowhere.

Three things follow, in order of how much they will cost somebody later:

- **A negative control has to fail on a device's first touch of a page**, which is a
  read. I2's is "a present context entry over an empty domain": the walk finds an
  all-zero second-level entry and refuses the read. QEMU answers that with fault
  reason `0x06` (read permission) rather than a separate not-present code — its own
  line is `detected sspte permission error (iova=0x1000000, level=0x4, sspte=0x0,
  write=0)` — so the gate asserts the reason is one a *second-level walk* decides
  rather than naming one code.
- **I4's permission-narrowing paths are not affected, because §5.5 already forbids
  the shape that would hit this.** Narrowing is "as unmap": write the entries, flush
  them, then invalidate — and an invalidation empties the entry that would have
  masked the fault. A branch that skipped the invalidation would be silently
  *correct-looking* here, which is one more reason §5.5 refuses to have one.
- **The boot's own silence is a second instrument defect worth knowing about.** The
  kernel log ring is drained only by the timer tick and the idle loop, and during boot
  the timer is not armed and the idle loop has not been entered — so a boot that
  wedges before either produces *no serial output at all*, including everything it
  logged before wedging. Only the fatal paths flush (`serial::panic_flush`). Anyone
  bisecting a boot wedge should put a `serial::flush_final()` in `log!` first; that is
  how this one was found.

Not measured, and the next stage's to answer: **whether QEMU's virtio devices are
behind the unit at all.** §5.3 records that they bypass it unless created with
`iommu_platform=on`, and `iommu_platform=on` needs the guest to negotiate
`VIRTIO_F_ACCESS_PLATFORM`, which this kernel's virtio drivers do not. Under identity
mapping a bypassing device and a translated one are indistinguishable, so the suite
being green says nothing either way. It is why both of I2's gates run on
[`Profile::Metal`], whose devices are all ordinary emulated PCI functions.

---

## 9. Stages

Every stage leaves the tree green: `cargo run -- --build-only` clean, `cargo test`
green including gate A's fast tier. Exit criteria are runnable commands, not
descriptions.

| Stage | Content | Exit criterion |
|---|---|---|
| **I0** | **Harness first.** An `iommu` dimension on `Shape` in `tests/common/qemu.rs` and `src/qemu.rs`; every profile passes `-device intel-iommu,intremap=on,caching-mode=on,aw-bits=48` and `-machine q35,kernel-irqchip=split`. No kernel change. Proves OVMF and the current kernel tolerate the device before anything depends on it. | `cargo test` green with the flag on every profile |
| **I1** | **Discovery, read-only.** `acpi::find_table` made public; `kernel/src/iommu/{mod,vtd/dmar}.rs`; DRHD inventory, register windows mapped, `CAP`/`ECAP`/`VER` decoded, one log line per unit. Refuses nothing. | `cargo test -- iommu_discovery` — asserts the capability line under the flag and the distinct no-DMAR line without it |
| **I2** | **Translation on, one identity domain** (not "everything passthrough" — §8.1). Root/context tables for every enumerated function, second-level tables over `[0, top)`, queued invalidation, the fault-event MSI, `SRTP`, `TE`. Behaviour unchanged by construction. | full `cargo test` green with `TES=1` asserted; plus `iommu_context_absent` and `iommu_empty_domain`, actuators that strand one function above and below the context entry and assert the boot dies with a DMA fault naming it |
| **I3** | **Interrupt remapping on.** Remappable IOAPIC RTEs and remappable `pci::enable_msi`/`enable_msix`; IRTA, `SIRTP`, `IRE=1`, `CFI=0`; every IRTE `SVT`-verified. | `cargo test -- metal_sim_input xhci audio nvme esp` green — every one of these depends on an interrupt arriving. Teeth: an IRTE written with the wrong source-id must red the corresponding test |
| **I4** | **Domains, mapping, invalidation, faults.** `create_domain`/`attach`/`map`/`unmap`/`flush`, the `Unmapped`/`Flushed` pair, IOVA allocator, fault MSI and the kill-on-fault path. No syscall yet; driven by an actuator self-test. | `cargo test -- iommu_selftest`, which maps, reads back through a device, unmaps, and asserts a stale access faults. Teeth: deleting the unmap-side invalidation must red it |
| **I5** | **The refusal.** Sequenced deliberately after `specs/plans/userspace-drivers-spec.md`'s first driver has moved, because before that a refusal costs every machine and protects nothing. | `cargo test -- iommu_refusal` — three variants (no `intel-iommu`; `intremap=off`; a unit whose `SAGAW` we reject via actuator), each asserting its own message and that userland is never reached |

**I0 and I1 are done**, `82a69a8..9eab2f2`. Every profile in `tests/common/qemu.rs`
and `src/qemu.rs` carries the unit; `kernel/src/iommu/` inventories it and refuses
nothing; `cargo test -- iommu_discovery` boots four machines differing only in the
unit and asserts the guest's decode moves with them. What I1 deliberately left for
later, so I2 does not have to rediscover it:

- **No `Iova`, `DomainId`, `DmaPerm`, `IommuError` or `trait Iommu`.** Discovery
  produces none of them, and a type with no constructor and no caller is the dead
  code §5.2 argues against. `AddressWidth` and `StreamId` are there because
  discovery produces both.
- **`AddressWidth` has two variants, not §3's three.** §5.3 takes the widest of 48
  and 39 and §11.5 rules 57-bit out, so `Bits57` would be matched nowhere.
- **A device scope whose path runs through a bridge yields no `StreamId`.** The
  requester id needs each bridge's secondary bus number out of its own config
  space; guessing would name a different device. Every scope QEMU publishes has a
  single-element path, and I2 has an ECAM window to do the walk with.
- **A DRHD register base is checked for 4 KiB alignment and the 52-bit physical
  ceiling, and not against the memory map.** A base pointing into usable RAM would
  read plausible garbage as a capability register — a wrong log line at I1, and a
  register write into somebody's heap at the stage that programs the unit.

**I2 is done.** Every profile in the suite boots with its unit translating; the
`gsts=…c4000000 tes=y qies=y` line is on every machine that has one, and both
actuator gates are green. What its exit criteria turned out to mean, where the text
above had said something else:

- **"Everything passthrough" is "everything identity-mapped"**, per §8.1, and I2 goes
  one step further than that finding: no passthrough context entry is written *ever*,
  even on a unit that offers one (§5.7). §5.2's argument against an arm no machine in
  reach executes applies to §5.7 as much as to `ECAP.C`.
- **The fault-reporting path is part of I2, not I4.** §9 listed the fault MSI under
  I4, but the I2 exit criterion asks for a boot that "fails with a DMA fault naming
  that function" — which is a fault handler. `FEDATA`/`FEADDR` are programmed and
  `FECTL.IM` cleared *before* `TE`, so the first transaction a unit blocks is one it
  can report. §7.1's split lands entirely on its first half here: every stream on the
  machine is kernel-owned, so every fault is a kernel bug and the handler logs the
  record and halts every CPU through `panic_console::capture` + `halt_all_cpus`, which
  is what puts the reason on the panel of a machine with no serial port.
- **§7.1's step 3, clearing Bus Master Enable on the offending function, is not
  implemented.** The next thing this handler does is stop the machine, so a device
  told to stop and a machine that has stopped are indistinguishable and the store
  would be a line no configuration in reach can show doing anything. It arrives with
  I4, when the handler stops halting.
- **§7.1's latch, counter, per-domain flag and `need_resched` handoff are also I4's.**
  They exist to bound a storm from a userspace driver and to kill the process that
  owns it; there is neither at I2.
- **The register-based invalidation path §4.2 allows is not written.** Every unit in
  reach has `ECAP.QI`, so `CCMD_REG`/`IOTLB_REG` would be the untested arm again. A
  unit without it is *not programmed* and says so, which is I5's refusal one stage
  early in everything but severity — the machine boots exactly as it does today,
  because a unit that is never enabled does nothing to DMA. The same is true of a unit
  offering no address width we implement, no 2 MiB superpage, too few domains, or
  fault recording registers that do not fit its own 4 KiB window.
- **`Iova` is in the portable seam; `DomainId`, `DmaPerm`, `IommuError` and
  `trait Iommu` are still not.** `Iova` earns its place because the identity policy
  needs exactly one site to live at (§5.7). The other four would each have one value
  or one implementor, which is the dead abstraction I1 argued against.
- **`ECAP.CM` and `ECAP.C` are still branchless, and now load-bearing.** Every table
  write goes out of the cache in the same operation that performs it — `Table::write`
  does the `clflush`+`mfence` and there is no way to call it without one — because
  §8.1 found `ECAP.C` clear on the machines the suite runs.

I0–I4 are independent of the capability-handles migration. I4's teardown path assumes
`process::exit` gains an explicit reclaim phase (§7.5); if
`specs/assessments/capability-handles-spec.md` stage B2 has landed, `DeviceClaim`'s
`on_zero_handles` is where step 1 of §7.5 lives and exit runs the rest. If it has not,
`device.rs`'s existing release path is. **Neither ordering blocks the other**; what is
not negotiable is that the slow half never runs from the deferred queue.

---

## 10. Failure modes

| Failure | Behaviour | Recovery |
|---|---|---|
| No DMAR / no usable unit | Halt with a named reason on serial and on the panel (§2.2) | User enables VT-d in firmware, or the machine is unsupported |
| Malformed DMAR (bad length, bad checksum, cyclic scope) | Refusal with the raw bytes named; never a panic | Same |
| Device DMA to an unmapped IOVA | Transaction blocked by hardware; BME cleared in the fault handler; one log line; owning process killed | init respawns the daemon; a fresh claim succeeds |
| Device DMA fault on a kernel-owned stream | The record is logged with the stream, address and reason; every CPU halts through the panic console. Built at I2, where every stream is one of these | Fix the kernel |
| A unit lacking a capability this kernel needs | Left unprogrammed, with a line naming the register. I5 turns each into a halt | The machine boots as it does with no unit at all |
| `GCMD` bit never appears in `GSTS` | Panic — a half-enabled unit has a reach nothing can state | None; hardware or programming fault |
| Device writes `0xFEE00000` | Compatibility-format interrupt blocked; interrupt-remapping fault; as above | As above |
| Driver fires its own IRTE in a loop | Counted, not logged; past the ceiling `IRTE.P` is cleared and the process is killed | As above |
| Driver process killed while its device is mid-DMA | §7.5: BME cleared inline; unmap/flush/release on the exit path | Pages return only after acknowledged invalidation |
| Invalidation not acknowledged within the bound | Panic — the kernel cannot know what the device can reach | None; this is a hardware or programming fault |
| IOVA space exhausted | `IovaExhausted` → `ResourceExhausted` to the caller | Driver unmaps; a driver that leaks is buggy |
| Interrupt remap table full | `ResourceExhausted` at claim time | — |
| PCIe device appears after boot with no context entry | Faults on first DMA, handled as any fault | Documented limit; PCIe hotplug is unimplemented. USB hotplug is not an instance: the DMA is the xHC's |
| Device shares an isolation scope | Refused at claim (`ScopeShared`), stays a kernel device | — |
| Device carries an RMRR | Refused at claim (`ReservedRegion`) | — |

No failure mode requires a scan, a timeout on device cooperation, or trusting a
driver. The only panics are for kernel bugs and for hardware that will not answer.

---

## 11. Explicitly not doing

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
8. **A passthrough context entry, or a branch on `ECAP.PT`.** §5.7, and the same
   argument: the only unit in reach has `ECAP.PT` clear, so the passthrough arm would
   be the one nothing executes. Read for the log line, never for a decision.
9. **Register-based invalidation** (`CCMD_REG`/`IOTLB_REG`). §4.2 allows it for a unit
   without `ECAP.QI`; every unit in reach has one, so it would be an untestable arm
   too. Such a unit is left unprogrammed and named, which is I5's refusal one stage
   early and leaves that machine exactly as it boots today.
10. **IORT parsing and any ARM code.** The seam admits an SMMU backend; this file
    writes none.
11. **AMD-Vi.** Same.
12. **A no-IOMMU fallback.** §2. This is the decision the rest of the file rests on.
13. **Per-device IOVA-space randomization.** IOVAs index a domain the owning process
    already exclusively holds; there is nothing to guess. (The same argument
    `specs/assessments/capability-handles-spec.md` §14.9 makes about handle values.)
14. **Reclaim on a dedicated kernel thread.** The kernel has no in-kernel thread
    mechanism today — `process::spawn_kernel` (`loader.rs:1119`) spawns a *userland*
    process — and adding one is a kernel addition needing its own discussion. §7.5's
    exit-path phase needs none.

---

## 12. Open risks

- **The `IRE` cutover (I3) can black-screen the machine** and its failure mode is
  "nothing works", which is the hardest kind to bisect. Mitigated by landing it alone,
  against a suite where every input, storage and audio test already depends on an
  interrupt.
- **Whether the harness's virtio devices are behind the unit at all is unknown**
  (§8.2). QEMU hands a virtio device the bypassing address space unless it is created
  with `iommu_platform=on`, and that option requires the guest to negotiate
  `VIRTIO_F_ACCESS_PLATFORM`, which this kernel's virtio drivers do not. Under I2's
  identity mapping the two are indistinguishable, so the whole green suite is evidence
  for neither. It stops being harmless at I4, where an IOVA is not a physical address:
  a bypassing virtio device handed one writes to whatever that number happens to be.
  The `VIRTIO_F_ACCESS_PLATFORM` negotiation is a virtio-driver change and belongs
  with whichever stage first needs a virtio device translated.
- **Isolation scopes (§7.3) are modelled, not measured.** The first real answer is the
  T14, and the answer could be that a device this project wants in userspace is not
  isolatable there.
- **RMRR on the T14 (§7.4)** could refuse a device for userspace that the plan assumed
  would move. Unknown until a real boot.
- **2 MiB DMA granularity (§5.4)** is coarser than every other IOMMU-based design, and
  it is inherited rather than chosen. If it ever becomes the limiting factor, the fix
  is a 4 KiB path in `mm/paging.rs` first, not a special case here.
- **`map_mmio` maps device registers write-back cacheable** (`mm/paging.rs:528-568`;
  the flag set is `PAGE_PRESENT|PAGE_WRITE` and there is no PCD/PWT/PAT bit anywhere
  in that file). It works today because firmware's MTRRs make the PCI hole
  uncacheable and the effective type is the stronger of the two — so the correctness
  of every MMIO access in this kernel rests on a mechanism the page tables do not
  state. Mapping a BAR into a *user* address space inherits the same dependency.
  Called out here because it is the sort of thing that is true until a machine where
  it is not.
- **Cost is unmeasurable in the harness.** The 2× bar in CLAUDE.md is answerable only
  on hardware, and the honest expectation is that a translated DMA path is slower than
  an untranslated one by an amount nobody here can currently quantify.
