# Userspace device drivers — Technical Specification

Moving device drivers out of the kernel and into ordinary processes, behind an
IOMMU. The IOMMU layer itself is `specs/iommu-spec.md`; this file is the
capability that spends it, the ownership rules that keep it honest, the staged
migration, and the mechanical check that says when it is done.

This came from an architecture-and-philosophy question, and it is argued on those
terms. It is not a performance change and it is not a code-size change — the
kernel gets smaller, but that is a consequence and not the reason.

## 1. The target

**`virtio` should not appear anywhere in the kernel.** Owner's words, and the
reasoning is that a hardware interface is policy about one device: it says what
descriptor format one vendor's NIC wants, and a minimal kernel has no business
holding an opinion about that. The word is a proxy for the property, and it is a
good proxy because it is mechanical.

Measured at `97dc6ee`, 2026-08-02:

```
$ grep -rn  virtio kernel/ --include='*.rs' | wc -l      89
$ grep -rni virtio kernel/ --include='*.rs' | wc -l     246
$ grep -rnil virtio kernel/ --include='*.rs' | wc -l     20
$ wc -l kernel/src/drivers/virtio{,_net,_gpu,_sound,_console}.rs
   496 virtio.rs   257 virtio_net.rs   682 virtio_gpu.rs
   648 virtio_sound.rs   220 virtio_console.rs   2303 total
```

2,303 lines of driver, plus roughly forty-five non-comment lines of coupling
elsewhere. Two of those forty-five need real design and the rest are deletions:

- **`kernel/src/audio.rs:4` is typed on the concrete driver** — `use
  crate::drivers::virtio_sound::SoundController` — not on a trait, unlike
  `net.rs`'s `trait Nic` (`net.rs:11`) and `gpu.rs`'s `trait Gpu` (`gpu.rs:20`).
  A const assert at `audio.rs:33` reaches into `virtio_sound::TX_INFLIGHT_MAX`.
- **`kernel/src/drivers/serial.rs` switches the kernel log backend to
  virtio-console** (`:71`, `:119-136`, `:202`, `:226`, `:230`). Fifteen lines,
  and §3.4 says what happens to them.

The remaining coupling is `drivers/mod.rs`'s five `pub mod` lines, `main.rs`'s
four `init` calls (`:455`, `:456`, `:458`, `:462`) and its panic-console
suppression (`:464-466`), two IDT vectors (`arch/idt/mod.rs:33-34`), and nine
comment-only mentions.

## 2. What the boundary buys, and when

Stated plainly because the obvious build order is wrong.

**Moving a driver to userspace, with no IOMMU, buys crash isolation and a smaller
kernel — and costs security.** A driver process that can write a descriptor
containing a physical address holds an arbitrary read/write primitive over all of
memory. In the kernel that is a kernel bug; in a process it is a privilege
escalation from any process that can claim the device. The kernel does not get
safer by moving code out of it if the code keeps the authority.

**Security isolation arrives only with the IOMMU, and interrupt remapping is part
of that, not a follow-up.** `specs/iommu-spec.md` §6.1 has the argument: a device
raises an MSI by writing `0xFEE00000`, which the platform decodes as an interrupt
rather than as a translated memory access, so DMA remapping never sees it — and a
driver that owns its BAR owns its MSI-X table, because the table lives inside a
BAR and this kernel maps at 2 MiB. Two escape routes, both closed only by
remapping with `CFI` disabled and every IRTE source-verified. A boundary that does
not bind what its name implies is worse than no boundary.

**Therefore the IOMMU lands first and completely, and no driver leaves the kernel
before it.** There is no phase in this plan that ships a userspace driver without
translation and remapping. That ordering is the single most important thing in
this document, and it is the opposite of what a "get something working" instinct
produces.

### 2.1 The uncomfortable fact underneath

This is not a hypothetical harm being avoided. **netd is already a userspace
driver with no IOMMU, today.**

`virtio_net.rs:224-225` registers a shared-memory token over the whole 2 MiB DMA
pool — `dma.phys() & !(PAGE_2M - 1)`, length `PAGE_2M` — and that pool contains
the RX queue's descriptor table, available ring and used ring (`OFF_RXQ_DESC`,
`OFF_RXQ_AVAIL`, `OFF_RXQ_USED`) and the TX queue (`OFF_TXQ`) as well as the
packet buffers. `device.rs:86-98` grants it to netd at `SYS_OPEN_DEVICE`, and
`SYS_MAP_SHARED` maps it **writable** into netd's address space
(`shared_memory::map` → `AddressSpace::alloc_and_map(phys, size, true)` →
`PAGE_USER|PAGE_WRITE`). A virtqueue descriptor's address field is a physical
address the device dereferences.

The tree already contains the fix, one file over: **virtio-sound allocates two
pools** — `dma_kernel` for the virtqueues and `dma_shared` for what soundd may
see (`virtio_sound.rs:515-516`, `:561` registering only the second) — and
virtio-gpu publishes framebuffer and cursor pages, never its ring pool. Only
virtio-net publishes the rings.

Read off the code, **not staged**, and filed as such in
`specs/issues/isolation/netd-writable-virtqueue.md` with the reproduction
written out. It is here because it settles the framing:
the question is not whether to expose a DMA engine to userland. That has already
happened. The question is whether the thing it can reach is bounded.

## 3. The exception set: which drivers stay in the kernel

### 3.1 The criterion

**A driver stays in the kernel only if the kernel needs it while userspace is
dead.**

"Non-trivial" was considered and rejected. It is not a criterion — it drifts, and
it selects backwards: it would keep NVMe, xHCI and USB mass storage, which are the
large attack surfaces, and move virtio-net, virtio-sound and virtio-gpu, which are
the small ones. The criterion above selects on what the kernel's own guarantees
depend on, which is a property of the design rather than of a line count.

### 3.2 What it selects today

| Component | Stays? | Why |
|---|---|---|
| `drivers/serial.rs` (16550) | **stays** | The kernel's log backend before any process exists. |
| `drivers/gop.rs` + `drivers/panic_console/` | **stays** | The on-screen panic console is the only diagnostic channel on a machine with no serial port. Firmware-configured rather than driven — `gop.rs` is 82 lines and maps a framebuffer address the bootloader passed in. |
| `drivers/log_ring.rs`, `esp_log.rs` | **stays** | Not drivers. The log. |
| `drivers/xhci/*` + `drivers/usb_storage.rs` | **stays** | The path `esp_log` writes through. `fat32_adapter.rs:680-685` resolves `/boot` only through `usb_storage::open`, on every machine shape, and `src/qemu.rs:97-109` puts the boot stick on the xHCI in every profile. This is the guarantee that a wedged machine leaves its own explanation on the stick, and it is the only reason the T14 is debuggable at all. |
| `drivers/xhci/hid.rs` | **stays, as a consequence** | USB HID rides the same controller, the same event ring and the same lock as the boot disk. **You cannot move USB HID to userspace without moving the boot block device or splitting the controller**, and neither is available. This is the sharpest constraint in the whole plan and it was not obvious before the code was read. |
| `drivers/pci.rs`, `drivers/acpi.rs`, `drivers/ioapic.rs`, `arch/idt/`, `irq_ring.rs`, `iommu/` | **stays, and is not a driver** | Bus enumeration, firmware tables, the interrupt controller, the IOMMU. These are the machine, not a device. A minimal kernel arbitrates devices; it must therefore be able to see them. |
| `drivers/nvme.rs` | **stays — see §3.3, where the criterion is widened, loudly** | |
| `i8042/` | **leaves, last** | The kernel does not need a keyboard while userspace is dead — today. §6 stage 10, and it needs a port-IO capability, which is its own discussion. |
| the four virtio drivers + `virtio.rs` | **leave** | Nothing the kernel guarantees depends on them. |

### 3.3 Where I widen the criterion, and why

Read strictly, the criterion **does not keep NVMe.** `esp_log` never touches it:
`/boot` is USB mass storage over xHCI, always, and NVMe is claimed by
`page_cache::init` at `main.rs:371` to back `/home`. So the letter of the rule
says NVMe leaves, and the filesystem follows it out of the kernel.

That is a real conclusion and it should not be reached by accident. The kernel
today provides a VFS as one of its own services (CLAUDE.md: "resource management,
scheduling, process lifecycle, filesystem, device arbitration"). A service the
kernel provides needs a device while userspace is dead in exactly the same sense
the log does.

**Proposed wording, for the owner to accept or reject:** *a driver stays in the
kernel only if a service the kernel itself provides needs it while userspace is
dead.* That keeps NVMe as the VFS's device and keeps xHCI/USB-MSC as the log's,
and it changes nothing else in the table.

The alternative — keeping the original wording — is coherent and larger: it makes
"move the filesystem to userspace" a prerequisite for NVMe leaving, which is a
much bigger project than this one and should be decided on its own merits rather
than as a side effect of a device-driver rule. Either answer is defensible; what
is not defensible is leaving the ambiguity in the file.

Note that the exception is **per role, not per driver**: it keeps *the driver of
the volume the kernel logs to*, not "USB". A second USB disk, or a second NVMe
namespace, is not the boot volume and can be handed to a userland driver through
the same capability as anything else. The kernel's copy of the driver existing is
not a reason for userland not to have its own.

### 3.4 virtio-console: the case that contradicts the target

Under the criterion, `virtio_console.rs` **stays** — it is a kernel log backend
(`serial.rs:119-136` switches to it once init completes) and the kernel needs a
log backend while userspace is dead. Under the target, it must go, because it says
`virtio`.

The contradiction is real and the resolution is to **delete it**, not to move it.
The kernel's guaranteed channels are the 16550 where one exists, the GOP panic
console, and `esp_log` through the boot volume. virtio-console is redundant with
the 16550 on every QEMU profile that has one, and it does not exist on the machine
this project targets — the T14 has no virtio device of any kind, which is exactly
what `Profile::Metal` exists to model. Deleting it moves the QEMU console onto the
16550, which makes the development shape *closer* to the hardware shape, and
CLAUDE.md says deleting code wins on that basis alone.

One thing to measure rather than assume before the deletion lands: boot time and
log throughput on the 16550 against virtio-console, same session, A/B. If the UART
is materially slower under TCG, that is a development-ergonomics cost and the
owner decides. Recorded as stage 8's gate.

## 4. The capability

What a userspace driver needs, and nothing more. Five things.

CLAUDE.md forbids adding a syscall without discussion, so everything in §4.6 is a
**proposal awaiting the owner's approval**, not a decision.

### 4.1 Enumeration and identity

A driver has to find its device and read what kind it is. `pci::enumerate`
(`pci.rs:250`) already walks the bus once for the whole kernel and returns every
function; the capability exposes a read-only projection of that list.

**Claiming is not first-come.** `SYS_OPEN_DEVICE` is first-come and ungated today
(`specs/issues/isolation/`: "a process that beats the daemon to a device, or claims it
after the daemon dies, holds everything the claim unlocks"), and reproducing that
for something that hands out a DMA engine would be a strictly worse version of an
already-open defect. A claim requires a right on a `SysCap` handle granted by init
from `system.toml`, per `specs/assessments/capability-handles-spec.md` §6.7. The device a
process may claim is named in the manifest; the kernel does not guess.

### 4.2 Config space

Read-only, as a snapshot of the standard 4 KiB. Nothing dangerous is *read* from
config space — it is device identity — and the driver needs it to parse its own
vendor's capability list. **This is the mechanism by which `virtio` leaves the
kernel:** the kernel hands over "here are your BARs and here is your config space",
and knowing what a virtio-pci capability structure means is the driver's problem.

No write path in v1. The registers a driver might want to write are exactly the
ones it must not: BAR base addresses (they would move out from under the kernel's
mapping), the command register's Bus Master Enable (the kernel's only reliable way
to stop a device — §5), the MSI/MSI-X capability (the kernel owns interrupt
binding), and PCIe Device Control's FLR bit. If a real driver needs a specific
write later, it gets a specific syscall with the offset named in the kernel, not a
generic escape hatch.

### 4.3 BAR mapping, and the 2 MiB problem

`pci.rs:107`'s `read_bar_64` reads a BAR's address and **never sizes it** — there
is no write-all-ones/read-back anywhere in the file, because every kernel driver
hardcodes the window it wants (`nvme.rs:426` maps 0x4000, `xhci/mod.rs:814` maps
0x10000, `virtio.rs:85` maps 0x4000). A userspace BAR-mapping syscall cannot
hardcode anything, so BAR sizing is new work.

The harder problem is granularity. **ToyOS maps at 2 MiB and nothing else**
(`mm/mod.rs:17`; every function in `mm/paging.rs` asserts 2 MiB alignment). BARs
on a packed PCIe bus are commonly 4 KiB to 64 KiB and adjacent, so mapping one
device's BAR at 2 MiB granularity maps its *neighbours'* registers into the same
process. That is an isolation hole in the thing whose entire purpose is isolation.

Three options, and the third is the one this plan takes:

1. **4 KiB pages for MMIO mappings only.** Contradicts the kernel's one page size
   and adds a second mapping path to `mm/paging.rs`.
2. **Refuse any BAR that shares its 2 MiB region with another function's BAR.**
   Honest, and likely refuses everything on a real bus.
3. **Re-assign the BARs of any function handed to userspace onto 2 MiB
   boundaries.** The OS may reprogram BARs after firmware; ToyOS has no ACPI
   resource manager to conflict with, and the 64-bit prefetchable window has room
   to spare. Kernel-owned devices keep firmware's assignment untouched.

(3) is what a system with no legacy can do and an older one cannot. Option (2)'s
check is kept as the **assertion** that (3) worked: after re-assignment, a mapped
BAR whose 2 MiB region contains any other function's BAR is a refusal, so the
property is enforced rather than intended.

Two things stated rather than assumed:

- **The MSI-X table is inside the mapped BAR and the driver can write it.** Not a
  leak to be plugged — it cannot be plugged at this granularity — but the second
  reason interrupt remapping is load-bearing (`specs/iommu-spec.md` §6.1).
- **`map_mmio` maps write-back cacheable** (`mm/paging.rs:528`; the flag set in
  that file is `PAGE_PRESENT|PAGE_WRITE|PAGE_USER|PAGE_SIZE_BIT` and there is no
  PCD/PWT/PAT bit). It works because firmware's MTRRs make the PCI hole
  uncacheable and the effective memory type is the stronger of the two. Mapping a
  BAR into a user address space inherits that dependency. Recorded in
  `specs/iommu-spec.md` §11 as an open risk; it is the kind of thing that is true
  until a machine where it is not.

### 4.4 DMA memory and IOVAs

**Userspace never sees a physical address and never chooses an IOVA.** Both halves
matter and they close different holes.

The syscall takes the caller's own virtual range. The kernel resolves it through
the calling process's page tables (`AddressSpace::translate`, `paging.rs:392`), so
a driver can map only memory it already owns — there is no argument in which it
could name somebody else's. The kernel then allocates an IOVA from that domain's
own allocator and returns it. The driver puts that number in a descriptor and
never learns what it points at.

Consequences, each deliberate:

- **A physical address never crosses the ABI**, in either direction. Today it does
  not either, but only because the shm token idiom hides it; here the type says so.
- **An IOVA is not a capability.** It names a location inside a domain the calling
  process exclusively holds. Leaked to another process it grants nothing, because
  that process cannot program the device. This is the question
  `specs/issues/isolation/` says to ask of every new syscall taking a raw id, asked and
  answered rather than skipped.
- **The range must be resident.** A demand-paged VA with no physical page behind it
  cannot be mapped; the kernel faults the pages in or returns `InvalidArgument`.
  Untrusted input, so never a panic (CLAUDE.md's fail-fast corollary).
- **A DMA mapping pins its pages.** The mapping holds the `PhysPage` values, so
  `SYS_MUNMAP` of a DMA-mapped range cannot return them to the PMM while the device
  can still write them. This is exactly the `SYS_PIPE_MAP`-mapping-outlives-its-page
  defect (`specs/issues/isolation/`) with a NIC instead of a process, and it is closed
  the same way `specs/assessments/capability-handles-spec.md` closes that one: by a reference
  that keeps the thing it names alive.
- **Granularity is 2 MiB**, inherited from the kernel's page size
  (`specs/iommu-spec.md` §5.4). A driver that wants a 4 KiB ring maps 2 MiB and
  uses part of it; the device can reach the whole 2 MiB. That is weaker than a
  4 KiB-granular IOMMU and it is the price of the kernel's one page size.

### 4.5 Interrupts as wakeups

The kernel keeps a per-vector stub. It is not a driver and the distinction has to
be structural, or "the kernel has no drivers" becomes a lie: **the stub reads no
device register.** It timestamps, publishes to `irq_ring` (`irq_ring.rs:60`), sets
`need_resched`, and EOIs — which is exactly what the existing virtio-net and xHCI
ISRs already do (`arch/idt/virtio_net.rs:7`, `arch/idt/xhci.rs:7`). The scheduler
pass converts the record into an io_uring completion on the driver's device handle
(`sched/driver.rs:558-592`, the shape already in use for `Source::Network` and
`Source::Audio`).

That the stub reads nothing is only affordable because **MSI/MSI-X is required**.
An MSI is edge-triggered and needs no device-side acknowledgement; a level-triggered
INTx line must be masked at the source or it re-asserts forever, and masking it
means the kernel touching the device. So: **a function offering neither MSI-X nor
MSI is not eligible for userspace.** `pci.rs` already refuses such a controller by
name for xHCI (`specs/issues/hardware/`, "a function offering neither is not
initialised at all"), so the shape exists.

Two pieces of existing work this needs:

- **`pci.rs:138`'s `enable_msix` programs only entry 0**, and `MSG_ADDR` is
  `0xFEE0_0000` with physical destination **0** (`pci.rs:36`) — every device
  interrupt in this kernel lands on the boot CPU. Multi-vector userspace drivers
  need entry-N programming and a real destination, and both are wanted anyway.
- **`irq_ring::IrqSource` is a four-variant closed enum** (`Audio, Net, Xhci,
  I8042`) whose exhaustiveness is the point. A dynamic set of userspace-driver
  vectors needs a fifth variant carrying a binding index, not the enum being
  opened up.

### 4.6 Proposed syscalls

```
SYS_PCI_LIST(syscap_h, out_ptr, cap)      -> n            // read-only; DEVICE right
SYS_DEVICE_CLAIM(syscap_h, bdf)           -> handle       // creates the domain, attaches,
                                                          // enables BME; exclusive, no DUP
SYS_DEVICE_CONFIG(h, out_ptr)             -> ()           // 4 KiB config snapshot, read-only
SYS_DEVICE_BAR(h, index, out_ptr)         -> ()           // {present, size, kind}
SYS_DEVICE_BAR_MAP(h, index)              -> vaddr        // MAP right; 2 MiB-aligned (§4.3)
SYS_DMA_MAP(h, vaddr, len, perm)          -> Iova         // caller's own range; kernel resolves
SYS_DMA_UNMAP(h, iova, len)               -> ()           // unmap + synchronous invalidate
SYS_DEVICE_IRQ(h, index)                  -> handle       // an io_uring-pollable source
```

Deleted by the same change, once their drivers have moved: `SYS_NIC_RX_POLL`,
`SYS_NIC_RX_DONE`, `SYS_NIC_TX`, `SYS_AUDIO_SUBMIT`, `SYS_GPU_PRESENT`,
`SYS_GPU_SET_CURSOR`, `SYS_GPU_MOVE_CURSOR`, `SYS_GPU_SET_RESOLUTION`. Every one
of them is a device protocol in the syscall table — eight syscalls whose existence
is the same defect the `virtio` grep is a proxy for. **That deletion is a better
end-state argument than the line count**, because a syscall number is ABI and a
driver file is not.

There is deliberately **no** `SYS_DEVICE_RELEASE`: closing the handle is release,
and process exit drains the table.

## 5. Ownership and teardown

This is where the plan earns its keep, and where this codebase has been bitten
repeatedly.

### 5.1 The two bugs to make unrepresentable

- **Unmap without invalidate.** The device keeps a cached translation for a page
  the PMM has reused. `specs/iommu-spec.md` §5.6 has the type: `unmap` returns a
  `#[must_use] Unmapped` whose pages come out only through
  `release(&Flushed)`, and `Flushed` is constructible only by `Iommu::flush`.
  Releasing with no invalidation at all is unrepresentable; releasing against the
  wrong range is a runtime assert, because ranges are values.
- **A mapping outliving its pages.** Closed by the mapping owning them (§4.4).

### 5.2 Drop does not run, so nothing depends on it

CLAUDE.md's caveat applies directly and it is the reason most of this section
exists. **This kernel does not unwind.** A driver process killed by another CPU has
its frames discarded without destructors running, so a `Drop` guard on the victim's
stack constrains nothing on the path that matters. `toyos-sched`'s `Registration`
drop bomb is the worked example of getting this wrong.

Asked of every type here, as CLAUDE.md requires: *which paths does this bind, and
is the failing one among them?*

- `Unmapped`/`Flushed` bind **ordering inside one function on the reclaim path**.
  Reclaim runs on the killer's or the exiting thread's own stack, which is a live
  context. The victim never holds an `Unmapped`. Dropping one leaks pages, which is
  the safe direction, and `#[must_use]` catches the accident at compile time.
- **That teardown happens at all** is bound by neither. It is bound by the process
  death path being *code that runs*.

### 5.3 Teardown is explicit, split, and driven from the death path

```
inline, bounded — from the device claim's zero-handle hook:
  1. clear IRTE.P for every binding      (one store per vector; the driver
                                          cannot undo it, the IRTE is kernel memory)
  2. clear Bus Master Enable             (one 32-bit ECAM store; the function
                                          stops issuing memory requests here)

deferred, unbounded — an explicit phase of process::exit:
  3. Function Level Reset where supported, and wait
  4. unmap every range in the domain                    -> Unmapped
  5. invalidate IOTLB + context cache, wait for ack     -> Flushed
  6. Unmapped::release(&Flushed)                        -> pages to the PMM
  7. detach the stream, destroy the domain
```

Steps 1–2 are two MMIO stores and are the safety-critical ones: after them the
device is inert. Steps 3–5 are not bounded — PCIe allows up to 100 ms for an FLR —
which is why they are **not** on the deferred zero-handle queue.
`specs/assessments/capability-handles-spec.md` §5.2 drains that queue at syscall exit, at
`do_schedule` entry, and **in the idle loop**, and putting an uninterruptible device
operation in front of `pass()` is precisely the `esp_log` defect
(`specs/issues/boot-media/`: 2.0–9.7 ms per flush against a 23.219 ms audio pipeline,
and it is still the residual under gate A's red run). One instance of that mistake
in the tree is enough.

`process::exit` therefore gains an explicit `iommu::reclaim` phase, between
"drop the drained handles" and `detach_user` — `specs/assessments/capability-handles-spec.md`
§9.1 steps 4 and 5. It runs on a thread that is alive and may block.

**Pages return to the PMM only at step 6.** A crashed driver leaks memory for the
duration of its own teardown and never hands back a page a device can still reach.

### 5.4 Relationship to the capability-handles spec

Not a hard blocker, and worth stating precisely because "blocked on another spec"
is how work stops. Device claims today are a `Lock<Option<Pid>>` per class
(`device.rs:12-16`) released from the fd-table drain at exit, which is already the
death path rather than a `Drop` on the victim's stack — so this plan can be built
on today's mechanism.

`specs/assessments/capability-handles-spec.md`'s `DeviceClaim` object (§6.5) is where this is
going: per-claim, created without `DUP` so duplication is unrepresentable,
`on_zero_handles` firing steps 1–2 above. If stage B2 of that spec has landed when
this work starts, use it. If not, `device.rs` is the home and the migration to
`DeviceClaim` is mechanical.

What is **not** negotiable either way: the claim is not first-come (§4.1), and the
slow half of teardown never runs from the deferred queue (§5.3).

## 6. The migration

Stages, each leaving the tree green: `cargo run -- --build-only` clean and
`cargo test` green including gate A's fast tier. `specs/plans/iommu-plan.md`'s
stages up to I4 are prerequisites for stage 5 here and can proceed in parallel
with stages 1–4.

| Stage | Content | Exit criterion (runnable) |
|---|---|---|
| **1** | **Trait-ify `audio.rs`.** `trait Audio` beside `net.rs`'s `trait Nic` and `gpu.rs`'s `trait Gpu`; `audio.rs:4`'s concrete `SoundController` and `:33`'s const assert go behind it. Pure relocation, independently valuable, no IOMMU involved. | `cargo test -- audio` green; gate A fast tier green |
| **2** | **Fix the netd exposure (§2.1).** virtio-net gets the two-pool split virtio-sound already has: rings kernel-only, buffers published. Independent of everything else here and it closes a live hole. | a new `abuse_nic_ring` test in `tests/toyos-rust-tests/` writes the descriptor table through the mapped token and asserts the write is not possible; gate N's pcap shows no crafted frame |
| **3** | **BAR sizing and 2 MiB re-assignment (§4.3).** `pci.rs` grows a sizing path and a relocation path used only for functions destined for userspace; the overlap assertion. Kernel drivers untouched. | `cargo test -- pci_bar` — asserts sizes read back, that a relocated BAR still responds, and that the overlap check refuses a deliberately-packed pair |
| **4** | **The capability, with no driver behind it.** §4.6's syscalls, the domain object, the IOVA allocator, teardown. A profile gains a **second** `virtio-net-pci` on its own netdev that the kernel does not claim, and a test binary claims it and exercises map/unmap/fault/crash-reclaim without implementing any protocol. **This is where the hard gates live** — §7 — and they land before any driver moves, which is `specs/device-test-strategy.md`'s "device shape and lifecycle before protocol depth" applied to the capability itself. | `cargo test -- usbdrv_` — the whole §7 set |
| **5** | **virtio-net moves.** A userspace virtio-pci + virtio-net driver in `userland/`, consumed by netd; `iommu_platform=on` on that device; `kernel/src/drivers/virtio_net.rs`, `arch/idt/virtio_net.rs`, vector 0x22, `net.rs`'s registry and `SYS_NIC_*` all deleted. | gate N's fast tier (`specs/plans/net-gate-plan.md`), plus §7's gates re-run against the real driver |
| **6** | **virtio-gpu moves.** Compositor. `virtio_gpu.rs`, `gpu.rs`'s registry and the four `SYS_GPU_*` syscalls deleted; `main.rs:464-466`'s panic-console suppression goes with it. | `cargo test -- screen compositor`; the panic-console tests are the ones that must not move |
| **7** | **virtio-sound moves.** soundd. `virtio_sound.rs`, `audio.rs`'s registry, `SYS_AUDIO_SUBMIT`, vector 0x23 deleted. | **gate A's thorough tier**, `cargo test --test toyos-build -- --audio-gate 30`, same-session A/B against the pre-stage tree. Same rule as a scheduler-migration stage transition, and for the same reason: this is the one stage that can move a latency distribution |
| **8** | **virtio-console deleted (§3.4).** Console on the 16550 for every profile; `serial.rs`'s fifteen lines and `virtio.rs` itself go. | full suite; boot-time and log-throughput A/B recorded in the commit message |
| **9** | **The refusal.** `specs/plans/iommu-plan.md` stage I5. Sequenced here because before this point a refusal costs every machine and protects nothing that has moved. | `cargo test -- iommu_refusal` |
| **10** | **i8042 moves** — needs a port-IO capability, and that is a separate discussion: an unrestricted IOPB would reach 0xCF8/0xCFC and every other port on the machine, so it has to be a per-port bitmap. Optional; the exception-set criterion permits the i8042 to stay if a "press a key to page the panic screen" feature is ever wanted, because that is the kernel needing a keyboard while userspace is dead. | `cargo test -- metal_sim_input i8042` |
| **11** | **Audit and the end condition.** §7.4's mechanical checks in CI; CLAUDE.md architecture updated; `specs/issues/` entries closed. | the §7.4 commands, in CI |

Stage 2 is worth landing first regardless of whether the rest of this plan
proceeds.

## 7. Testing

Per `specs/device-test-strategy.md`: ground truth at the hardware boundary, the
harness as actuator, device shape and lifecycle before protocol depth. And per
`specs/assessments/metal-track-history.md`: **teeth are necessary and not sufficient** —
mutating the implementation tests the paths written, never the states not thought
of — so the state-space attacks are listed separately from the teeth.

### 7.1 The gate that matters most

**A driver that writes an unmapped IOVA must fault, must be blocked, and the test
must prove the write did not land.** Asserting on a log line alone would pass on an
IOMMU that reports faults and translates nothing.

Two arms, and the test is worthless without both:

- **Negative.** The driver programs an RX buffer at an IOVA the kernel did not map.
  The harness sends a frame. Assertions: an interrupt-free page canary is intact,
  the fault counter incremented, the driver process was killed, and the frame is in
  the host's `filter-dump` pcap (so the harness knows the frame was really sent and
  the absence of a write is not the absence of a packet).
- **Positive.** The same driver, the same page, the IOVA *mapped*. The frame's bytes
  appear in the guest's buffer, byte-exact against a seeded payload.

The canary needs an actuator (`iommu-canary`): the harness must know
which physical page an unmapped IOVA would resolve to if translation were bypassed,
and only the kernel knows that. The actuator maps a canary page, reports its
physical address, and hands the driver the numerically equal IOVA — deliberately
not in the domain. **The comment at that feature says why nothing else can reach
it**, per CLAUDE.md: the harness cannot read guest RAM, and the guest cannot learn a
physical address through any ABI this plan admits.

### 7.2 The vacuity traps, and how each is closed

Three ways this gate could go green having proved nothing. All three are real.

1. **QEMU's virtio devices bypass the vIOMMU by default.** Measured on this host,
   2026-08-02: `qemu-system-x86_64 -device virtio-net-pci,help` lists
   `iommu_platform=<bool> - on/off (default: off)`. With it off, QEMU uses the
   bypassing address space and no translation happens at all. Closed two ways: the
   profile must set `iommu_platform=on` on the device under test, **and** a boot
   assertion checks that the driver negotiated `VIRTIO_F_ACCESS_PLATFORM` — a
   host-side flag that the guest silently declines is the same defect as an
   argv-grep absence test (`specs/device-test-strategy.md`: "a profile that
   certifies 'no USB HID' by grepping argv for `usb-kbd` passes with a `usb-mouse`
   attached").
2. **An identity-mapped IOVA would make the positive arm pass with translation
   off.** Closed structurally by `specs/iommu-spec.md` §5.3: IOVA space starts at
   half the domain's address width, above physical RAM and above `0xFEE00000`, so
   an untranslated access to an IOVA cannot resolve to the right page. The test
   asserts the returned IOVA is above that base, which userspace can check and the
   host cannot fake.
3. **The negative arm passes if the device never DMAs.** Closed by the pcap: the
   frame reached the virtual wire. Ground truth at the hardware boundary, exactly
   as gate A's wav capture.

### 7.3 The rest of the set

| Test | Ground truth | What it certifies |
|---|---|---|
| `usbdrv_claim_lifecycle` | console text | claim, config snapshot, BAR map, IRQ bind, close; a second claim while live is refused; after close it succeeds |
| `usbdrv_claim_ungranted` | console text | a process with no `SysCap` DEVICE right cannot claim, and cannot enumerate (§4.1) |
| `usbdrv_dma_roundtrip` | pcap | positive arm of §7.1 |
| `usbdrv_unmapped_write` | canary + pcap | negative arm of §7.1 |
| `usbdrv_stale_iova` | canary | **the IOTLB use-after-free gate.** Map, get an IOVA, unmap, have the device access the stale IOVA: it must fault and the reused page must be intact. `caching-mode=on` makes QEMU's IOTLB a real cache, so a missing invalidation genuinely leaves a stale entry |
| `usbdrv_msi_escape` | interrupt counters | the driver points its device at `0xFEE00000`: an interrupt-remapping fault, no vector delivered, process killed. Negative control: the same test on an `intremap=off` profile must behave differently, or the assertion is measuring nothing |
| `usbdrv_msix_forge` | interrupt counters | the driver writes a foreign IRTE handle into its own MSI-X table: `SVT` source verification blocks it |
| `usbdrv_crash_reclaim` | pcap | kill the driver mid-DMA; a respawned driver claims cleanly and traffic resumes. This is the lifecycle test `specs/device-test-strategy.md` calls claim-then-die-then-reclaim |
| `usbdrv_map_not_owned` | error return | `SYS_DMA_MAP` over a VA the caller does not own, or a non-resident one, returns an error and never panics |
| `usbdrv_no_msi_refused` | console text | a function with neither MSI-X nor MSI is refused by name (§4.5); staged with `virtio-net-pci` and QEMU's MSI-X-disabling knobs |

### 7.4 Attacking the state space, not the implementation

Teeth (break it, watch it go red) are listed per test above and are necessary.
They are not sufficient, and the states below are the ones an implementation test
would never construct:

- **A device that is claimed, mapped, and then the process forks / spawns a
  child.** Which side holds the domain?
- **Two claims of two functions by one process.** Two domains, and an IOVA from one
  used in the other's descriptor must fault.
- **A `SYS_DMA_MAP` of a range that overlaps an existing mapping**, and of one that
  is adjacent to it.
- **Unmap of a sub-range of a mapping**, and of a range spanning two.
- **A driver that maps its own BAR mapping as DMA** — a device writing its own
  registers through the IOMMU.
- **A claim whose device disappears** (QMP `device_del` under active DMA). USB
  hotplug does nothing today; PCIe hotplug is unimplemented; the honest answer may
  be a refusal, and the test is what makes that answer explicit.
- **Reclaim racing a second claim** of the same device from another process.
- **A malformed DMAR** — the parser is fed crafted tables by an
  self-test, the way `xhci-xecp-selftest` feeds eight synthetic extended-capability
  lists (`specs/issues/hardware/`). Firmware is untrusted input.

### 7.5 The mechanical end condition

Three checks, in CI. The first is the owner's, the second and third exist because
the first can be satisfied dishonestly — a driver moved to userspace whose DMA is
still set up by a kernel helper called `net_dma_setup` passes a grep and keeps every
coupling.

```sh
# 1. The word does not appear in the kernel, in any case, including comments.
test 0 -eq "$(grep -rni virtio kernel/ --include='*.rs' | wc -l)"

# 2. The set of kernel drivers is exactly the exception set. The list is a
#    committed file, not a shell literal, so adding a driver is an edit to
#    kernel/src/drivers/EXCEPTIONS that a commit has to justify.
diff <(ls kernel/src/drivers) kernel/src/drivers/EXCEPTIONS

# 3. No device protocol survives in the syscall table.
test 0 -eq "$(grep -cE 'SYS_(NIC|GPU|AUDIO)_' toyos-abi/src/syscall.rs)"
```

`EXCEPTIONS` after stage 11 is `acpi.rs gop.rs iommu ioapic.rs log_ring.rs mod.rs
nvme.rs panic_console pci.rs serial.rs usb_storage.rs xhci`, plus `i8042` if
stage 10 is not taken and plus `EXCEPTIONS` itself. Written down because a list
whose contents are argued in prose is a list nobody can check.

Check 3 is the strongest of the three, because a syscall number is ABI and a
driver file is not.

## 8. Explicitly not doing

1. **A no-IOMMU mode.** `specs/iommu-spec.md` §2. The whole plan rests on it.
2. **Moving the boot storage path.** §3.2. It is the only reason a wedged machine
   explains itself.
3. **Moving USB HID separately from the boot disk.** §3.2 — it shares the
   controller, and splitting a controller between the kernel and a process is a
   worse idea than either half of it.
4. **A generic config-space write syscall.** §4.2.
5. **A port-IO capability**, except as stage 10's own discussion. §6.
6. **Kernel-side device protocol knowledge of any kind after stage 11.** No
   `virtio` shim, no "generic ring" abstraction in the kernel, no device class
   registry beyond claim arbitration. A generic abstraction over device protocols
   is the same mistake with a longer name.
7. **A driver framework in userland.** The first userspace driver is a program.
   The second one may share a crate with it. A framework before there are three is
   an abstraction with no evidence behind it.
8. **Hotplug.** Unimplemented today (`specs/issues/hardware/`), and this plan does not
   add it. §7.4 makes the refusal explicit rather than leaving it undefined.
9. **Restarting a crashed driver's device state.** init respawns the process; the
   device was reset at reclaim; the driver initialises it from scratch. There is no
   checkpoint and no partial-state recovery.
10. **A userspace filesystem.** §3.3 names it as the thing that would have to happen
    for the strict criterion to move NVMe, and explicitly does not propose it here.

## 9. Where I think part of this is wrong

A plan that cannot say no is not a plan.

- **The `virtio` grep is a proxy and it can be satisfied dishonestly.** §7.5 adds
  two checks that are harder to game, and the syscall-table one is the check I would
  keep if forced to pick one. The grep is a good *headline* because it is
  memorable and cheap; it is a weak *gate* on its own.
- **The refusal (§2, `iommu-spec.md` §2) is right and its default sequencing is
  wrong.** Landing it before any driver has moved costs every machine — every
  `cargo run`, every profile, every VM — and protects nothing, because at that point
  nothing outside the kernel touches a device. That is why it is stage 9 here and
  I5 there. This is a sequencing argument, not a disagreement with the decision.
- **The 2 MiB granularity is the weakest part of the design and it is not fixable
  here.** Both the BAR-mapping problem (§4.3) and the DMA-granularity one (§4.4)
  are the kernel's one page size showing through, and both would evaporate if
  `mm/paging.rs` grew a 4 KiB path. I am not proposing that — it is a change to the
  memory subsystem with its own consequences — but the honest statement is that the
  isolation this plan delivers is 2 MiB-grained, not page-grained, and every other
  IOMMU-based system in the world is the latter.
- **§3.3's widening should be the owner's call and I have made a recommendation
  rather than a decision.** The strict criterion is cleaner and much larger.
- **Stage 7 (audio) is the stage most likely to be reverted.** A driver crossing
  the kernel/user boundary adds at minimum a wakeup and a syscall to every period,
  and gate A's thorough tier at N=30 does not detect a doubling of the dropout rate
  (CLAUDE.md, and `specs/assessments/audio-gate-history.md`). So the gate can pass while the
  thing that matters got worse. If that stage looks marginal, the right answer is to
  leave virtio-sound in the kernel and fail the grep — a failed mechanical check
  with a recorded reason is better than a green one bought with audible quality,
  and quality tradeoffs are the owner's call, not this plan's.

## 10. Open risks

- **The `IRE` cutover can black-screen the machine** (`iommu-spec.md` §11). It is a
  prerequisite here, so this plan inherits it.
- **Isolation scopes and RMRRs on the T14** (`iommu-spec.md` §7.3–§7.4) could refuse
  a device this plan assumed would move. First real answer is a hardware boot.
- **Nothing here is measurable in the harness.** TCG's distortion is non-uniform, so
  the cost of a userspace driver against CLAUDE.md's 2× bar is answerable only on
  hardware or under KVM, same session, A/B.
- **The userspace virtio-net driver is new code with no upstream.** `specs/plans/net-gate-plan.md`'s
  instrument-defect lesson applies to it as much as to the analyzer: budget for the
  driver being wrong before the capability is.
- **Stage 5 removes the kernel's only NIC.** If the userspace driver is not working,
  the machine has no network — including sshd and gate N. Landing it means landing a
  working driver, not landing a capability and iterating on the driver afterwards.
