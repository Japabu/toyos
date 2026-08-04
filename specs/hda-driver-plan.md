# ToyOS HDA audio — the Intel High Definition Audio plan

The one audio device the one real machine has: `00:1f.3`, class `0403`,
`8086:a0c8`, prog_if `0x80`, "500 Series Chipset Family On-Package High
Definition Audio", listed **undriven** in `specs/metal-hardware-inventory.md:121`
and read off a boot that happened. **On metal today there is no audio at all**
— soundd prints `no audio device, presenting a null sink … streams discarded`
(`userland/soundd/src/main.rs:1548`), and none of gate A's guarantees describe
that machine.

This file is the shape of the whole track. It is a spec and nothing in it is
built. It is also the first device of `specs/userspace-drivers-spec.md`, so its
second job is the pattern: what a userland process needs in order to drive real
hardware, and which of those needs are the initiative's and which are this
device's.

**Everything measured here was measured on 2026-08-04** on this host, or is
transcribed from a document in this repository with its line cited. The method
is stated where the number appears. Estimates say so in the same sentence.

---

## 1. What this settles, and the one thing it cannot

Four questions, answered in §4, §5, §6 and §7. One is answered here because
everything else is downstream of it.

**Does HDA go in the kernel or in userland?** Userland, inside soundd, behind
the IOMMU. `specs/userspace-drivers-spec.md` §3.1's criterion — *a driver stays
in the kernel only if the kernel needs it while userspace is dead* — does not
keep audio under any reading, and §2 forbids a driver crossing the boundary
before the IOMMU is complete. `specs/wlan-plan.md` §3.1 already applied the
stronger form to a device *entering* userland rather than leaving it: no
kernel-resident interim driver at any point. The same rule binds here.

**The cost of that answer is the whole of §4.2**, and it is large: interrupt
remapping (I3), domains and mapping (I4), BAR sizing and relocation
(`userspace-drivers-spec.md` stage 3) and the capability itself (stage 4) all
land before the first HDA register is read. I3 is the step
`specs/iommu-spec.md` §12 calls the one that "can black-screen the machine".

**What this spec cannot settle** is whether the owner wants the last piece of
the doom milestone behind that chain. §4.3 prices the alternative — a
kernel-resident HDA driver behind a `trait Audio`, deleted later — honestly
enough that the owner can overrule §1's answer with data rather than with a
feeling. It does not propose it.

---

## 2. What HDA is, and what of it we need

Scope, fixed here and referred to throughout: **stereo S16 PCM out of the T14's
speakers and its headphone jack.** Everything else is §8.

Register offsets, verb identifiers and parameter numbers are **deliberately not
reproduced in this file.** `specs/iommu-spec.md` §7.1 gives the reason: a wrong
constant in a spec outlives every review. They come from the Intel High
Definition Audio specification at implementation time, and the code is where
they live.

### 2.1 The controller

One PCI function, one memory BAR. Measured on this host with QMP `query-pci`
against `-device intel-hda` on `-machine q35,kernel-irqchip=split`, QEMU 11.0.3:
vendor `0x8086`, device `0x2668`, class `0x0403`, **one BAR — index 0, memory,
16,384 bytes, not 64-bit, not prefetchable**. The T14's is `8086:a0c8`; its BAR
size, width and prefetchability are **not in any record here** and are H0's to
report.

What the driver touches:

- **`GCAP`, `VMIN`/`VMAJ`** — how many output, input and bidirectional stream
  descriptors this controller has, and whether it takes 64-bit addresses. Read
  once, logged once, and **never assumed**: a controller with fewer output
  streams than the driver wants is a refusal by name, not a truncation.
- **`GCTL.CRST`** — controller reset, released before anything else, then the
  codec-detect delay the specification requires.
- **`STATESTS`** — one bit per SDI link. This is the whole of codec presence
  detection, and §3 is why it is the single most load-bearing read in the plan.
- **`INTCTL`/`INTSTS`** — global and per-stream interrupt enables and status.
- **`CORB`/`RIRB` base, write and read pointers, control, size** — the command
  and response rings (§2.2).
- **One output stream descriptor** — `SDnCTL` (reset, run, interrupt-on-
  completion enable, stream number), `SDnSTS`, `SDnLPIB`, `SDnCBL`, `SDnLVI`,
  `SDnFMT`, `SDnBDPL`/`SDnBDPU` (§2.4).

Not touched: `WALCLK`, `SSYNC`, `WAKEEN`, the DMA position buffer base
registers, every input and bidirectional stream descriptor, and the vendor
extended capability block where the DSP lives.

### 2.2 Codec enumeration and the verb interface

A verb is a 32-bit command carrying a codec address, a node id and either a
12-bit verb with an 8-bit payload or a 4-bit verb with a 16-bit payload. A
response is 32 bits plus 32 bits of extended status (the codec address and
whether the response was solicited).

Two ways to send one, and the plan uses both, in this order:

- **The immediate-command registers.** One write, one poll, one read, **no DMA
  at all**. The specification defines them; whether a given controller or
  QEMU's model implements them is unmeasured here. H0 (§6) prefers this path
  because it is roughly fifty lines rather than a hundred and fifty, not because
  it is required — H0 is kernel code and can allocate a `DmaPool` like any
  driver, so CORB/RIRB is its fallback and the plan has no conditional in it.
- **CORB/RIRB.** The ring pair the driver uses in steady state: a 1 KiB command
  ring and a 2 KiB response ring, both DMA, both 128-byte aligned, both inside
  the driver's single 2 MiB DMA mapping (§4.4). **RIRB interrupts are not
  used** — the driver polls `RIRBWP` for the response to the verb it just sent,
  because verbs are issued only during enumeration and on jack polls, never in
  the audio path.

**Unsolicited responses are not enabled.** They exist so a codec can report a
jack change without being asked; §2.5 says why polling replaces them and what
that costs.

### 2.3 The widget graph, and the minimum traversal

The traversal, stated as an algorithm because this is the part QEMU certifies
least (§5.1) and the part most likely to be wrong on metal:

1. **Root node → function groups.** Read the root's subordinate-node count, and
   for each function group its type. Keep the **audio** function groups; log and
   skip the modem ones.
2. **Function group → widgets.** Its subordinate-node count gives the node id
   range. For each widget read its capability word: type (output converter,
   input converter, mixer, selector, pin complex, power, volume knob, beep
   generator, vendor-defined), channel count, and whether it has input or
   output amplifiers.
3. **Pin complexes → candidates.** For each pin complex read its **configuration
   default**. Discard pins whose port connectivity says "no physical
   connection". Keep the two default-device values this plan cares about:
   **speaker** and **headphone out**.
4. **Pin → converter.** Walk *backwards* from each candidate pin along its
   connection list — depth-first, bounded by the widget count, **and refusing a
   node already on the current path**, because a codec's connection list is
   untrusted input and a cycle in it must be a refusal rather than a hang.
   Stop at an output converter.
5. **Choose.** Prefer the pin whose configuration default names a speaker. If
   the graph offers a single converter feeding both a speaker pin and a
   headphone pin, take it — §2.5's routing wants both.
6. **Configure the path.** Power state D0 on the function group and every widget
   on the path; pin widget control set to output-enable (and headphone-amp
   enable on a headphone pin); the amplifiers on the pin and the converter
   unmuted and set to their 0 dB reference; the connection selector set on every
   selector and mixer along the path; the converter's format and its stream/
   channel assignment.

**Step 1 is where "multiple codecs deferred" becomes dangerous**, and it is the
sharpest thing in this section. The T14 has an Iris Xe iGPU at `00:02.0`
(`metal-hardware-inventory.md:105`), and display audio is a *codec on the same
HDA controller*. A driver that takes "the first codec that answers" can bind the
display codec, configure a perfectly valid output path, and produce silence from
the speakers with nothing in the log to say why. CLAUDE.md already records this
exact defect one layer down — "a first-match helper hides that choice, which is
how a machine with two identical controllers ends up with one driven". So:

> **Every codec `STATESTS` reports is enumerated, every audio function group is
> walked, and the one that yields a speaker pin is chosen. A machine where none
> does is a refusal that names every codec it found.** There is no first match
> anywhere in this driver.

### 2.4 Streams, the BDL, and how a completion is known

The output stream descriptor drives a **cyclic** buffer described by a Buffer
Descriptor List: an array of entries, each a 64-bit address, a byte length and a
flags word whose low bit requests an interrupt on completion. The list is
128-byte aligned, has at least two entries, and `SDnCBL` is the total cyclic
length.

The mapping onto ToyOS's existing audio shape is exact and is one entry per
period:

| ToyOS today | HDA |
|---|---|
| `TX_INFLIGHT_MAX = 8` DMA buffers (`virtio_sound.rs:241`) | 8 BDL entries |
| `PERIOD_BYTES = 512` (`virtio_sound.rs:121`) | each entry's length |
| the 2 MiB shared DMA page soundd maps | the buffers those entries point into |
| `audio_submit(idx, len)` per period | **nothing** — the engine wraps by itself |

**There is no submit in steady state.** `SDnLVI` is set once to the last index
and the DMA engine runs the ring forever. That is a materially cheaper path than
virtio's per-period descriptor submit — and it changes the underrun semantics in
a way that matters more than the saving:

> **A late soundd does not produce silence on HDA. It produces a *repeat* of
> whatever bytes are still in the buffer.** virtio-sound's device runs dry and
> stops; HDA's plays the stale period again.

Gate A's verdict is harm, and harm is defined as *silence that reached the
device* (`tests/audio-baseline.toml`, TWO TIERS). A repeated period is audible
harm that the gap detector cannot see. Two ways out, and the plan takes the
first:

1. **Zero the buffer at completion.** When the driver observes that period *i*
   has played, it zeroes buffer *i* before soundd fills it. The engine will not
   return to that buffer for seven periods, so the write is unraced. A period
   soundd fails to fill then plays as silence — **identical to virtio-sound at
   the harm boundary, so one instrument certifies both backends.** Cost: 512
   bytes of stores per 2.902 ms.
2. Teach the analyzer to detect a repeat. `specs/audio-gate-history.md` records
   a least-squares phase fit being used by hand to prove a generator stopped;
   making phase continuity an assertion would catch both a dropped and a
   repeated period. **Worth building anyway** (§5.3) — but as a second
   instrument, not as the reason the two backends may differ.

**How the driver knows which periods completed.** An interrupt covers one *or
more* buffers: soundd's `max_batch` counter exists because virtio already
batches, and `assert_eq!(free_mask & rec.mask, 0, "repeated completion for free
buffer")` in soundd's mix loop is the existing tooth on getting it wrong. So the
completion mask is **derived from a position read, never from counting
interrupts**: the ISR-side handler reads `SDnLPIB`, divides by the period size,
and marks every period between its own last-known index and that one.

The position source is `SDnLPIB` and **not** the DMA position buffer. Linux
carries a `position_fix` quirk family because on some controllers one is
trustworthy and on others the other; this project has no legacy obligation to
enter that swamp and no machine on which to test the arms. One source, and if
the T14 shows it untrustworthy the answer is to say so and switch — not to carry
both. The gate that would catch it is already written: soundd's assertion above
fires on a mask that repeats a buffer, and a mask that skips one shows as an
underrun.

**Position reporting beyond period granularity is not needed at all**, because
ToyOS's audio contract is `{mask, timestamp}` and not a sample position. That
deletes the whole of the `position_fix` family from scope, and it is worth
stating as a design property rather than an accident.

### 2.5 What is deferred, and what breaks if it is deferred wrongly

| Deferred | What it costs | What breaks if deferred *wrongly* |
|---|---|---|
| **Capture** (input converters, input pins, the ADC path) | no microphone, no recording | Nothing, if the driver refuses to configure an input path rather than half-configuring one. It also removes the only way a metal boot could verify its own output (§5.3). |
| **Multiple codecs** | — | **Everything.** §2.3: "deferred" must mean *enumerate all, choose by capability, refuse by name*, never *take the first*. This is the one deferral that is a defect if taken literally. |
| **Unsolicited responses** | jack changes are noticed on a poll, up to one poll interval late | Nothing. §2.3's jack polling replaces it. |
| **Jack detection itself** | — | **It is not deferrable and this plan does not defer it.** Without it the choices are speakers-only (headphones silent) or both-pins-always (speakers play while headphones are in). Neither is acceptable on a laptop. The pin-sense verb is two verbs on a 2 s timer — soundd already has a 2 s stats cadence to hang it on — so the cheap thing here is the correct thing. |
| **Power management** beyond D0-on-the-path | battery | Nothing, and it is the safe direction: a codec that pops on a power transition never makes one. soundd's §5.8 suspend-on-idle stops the *stream*, not the codec. |
| **HDMI / DisplayPort audio** | no sound over an external display | Nothing, provided §2.3's codec choice is by capability. |
| **Multi-channel, formats other than S16, sample rates the codec does not offer** | — | Nothing. soundd already converts rate and channel count per client, and `reject_open` already refuses a format it cannot serve (`soundd/src/main.rs:1234`). |
| **S/PDIF** | — | Nothing. |
| **The DSP / Smart Sound block** | no hardware offload, no vendor topologies | Unknown, and §3 is why: if the T14's speakers are *only* reachable through it, this plan does not work on that machine. |

---

## 3. The T14's hardware: what the record shows, and what it does not

**Shown**, from `specs/metal-hardware-inventory.md` and the verbatim boot log in
its appendix (line 572):

- The controller is `00:1f.3`, class `0403`, prog_if `0x80`, `8086:a0c8`,
  identified from `pci.ids` 2026.08.02 as "500 Series Chipset Family On-Package
  High Definition Audio". It is enumerated on every boot and no driver binds it.
- It is **function 3 of a five-function device**. `00:1f.0` eSPI `8086:a082`,
  `00:1f.3` HDA, `00:1f.4` SMBus `8086:a0a3`, `00:1f.5` SPI flash `8086:a0a4`,
  `00:1f.6` Ethernet `8086:15fc` (inventory lines 120–124). §7 risk 1 is
  entirely about that sentence.
- The machine has **no virtio device of any kind**, which is what
  `Profile::Metal` exists to model, so nothing in gate A's four recorded configs
  describes this hardware.

**Not shown, and not inferable from anything in this repository:**

- **The codec.** A codec is behind the HDA link, not on the PCI bus. No boot has
  taken this controller out of reset, so `STATESTS` has never been read and no
  codec's vendor or device id is anywhere in the record. Popular knowledge about
  what a ThinkPad T14 Gen 2 carries is not evidence and this file does not
  record a guess as a fact.
- **Whether the speakers are on the HDA link at all.** Tiger Lake designs exist
  in which the codec hangs off SoundWire and the legacy HDA link enumerates
  nothing. If that is this machine, `STATESTS` reads zero, the plan is dead on
  it, and no amount of driver work changes that. **This is the cheapest thing to
  learn and it is H0's first line.**
- **MSI.** `specs/userspace-drivers-spec.md` §4.5 makes a function offering
  neither MSI-X nor MSI ineligible for userspace. QEMU's `intel-hda` offers
  `msi=<OnOffAuto>` and no MSI-X option, measured today with `-device
  intel-hda,help`; what `00:1f.3` offers is unread.
- **BAR0's size, width and relocatability**, which `userspace-drivers-spec.md`
  §4.3's 2 MiB re-assignment depends on.
- **What the volume keys send.** §6's H8 depends on it and H0 answers it.

One property of the T14 that is *shown* and is load-bearing for §6's last stage:
**`/home` is a tmpfs on this machine** (`storage: /home is a tmpfs — it will not
survive a reboot`, appendix line 668), the NVMe disk is deliberately refused,
`/boot` answers `PermissionDenied` to userland writes, and `/log` is the log's.
**There is no writable persistent filesystem on the T14 today**, so volume
persistence is blocked on something outside this track.

---

## 4. Where the driver lives, and what the kernel must provide

### 4.1 One process, and it is soundd

**The HDA driver is a library crate linked into soundd, not a separate daemon.**

CLAUDE.md's architecture line is already this: "compositor, netd, soundd, sshd.
Each claims a device from the kernel, maps hardware buffers into its own memory,
and serves clients over IPC." soundd claiming `00:1f.3` rather than
`DEVICE_AUDIO` is that sentence working, not an exception to it.

The argument that decides it is the period budget. Today one period costs soundd
a wake plus two syscalls (`SYS_AUDIO_POLL`'s read of completion records,
`SYS_AUDIO_SUBMIT`). With the driver in soundd it costs **a wake and no
syscalls**: the completion mask comes from an MMIO read of `SDnLPIB` in soundd's
own address space, and there is no submit at all (§2.4). A *separate* driver
daemon would put an IPC round trip inside a 2.902 ms period, which is the shape
`specs/userspace-drivers-spec.md` §9 predicts will get stage 7 reverted.

So the boundary move makes the audio path **shorter**, not longer. That is worth
stating plainly because the opposite is the intuition.

What would reopen it, recorded now so the decision is falsifiable later
(`wlan-plan.md` §3.2, same discipline): a second audio device; a capture path
whose lifetime differs from playback's; or a measurement showing the MMIO reads
from a user address space cost more than the syscalls they replaced.

### 4.2 The prerequisite chain

Not this track's work, and named so the dependency is visible. Shared with
`specs/wlan-plan.md` W5 — neither track pays for it alone.

| Prerequisite | State | What HDA needs from it |
|---|---|---|
| `iommu-spec.md` **I0–I2** | **done** | every function has a context entry and translation is on |
| `iommu-spec.md` **I3** — interrupt remapping | not built | HDA's MSI must be deliverable to a userland driver at all |
| `iommu-spec.md` **I4** — domains, map/unmap/invalidate, faults | not built | the CORB, RIRB, BDL and audio buffers are IOVAs |
| `userspace-drivers-spec.md` **stage 3** — BAR sizing and 2 MiB re-assignment | not built | `read_bar_64` never sizes a BAR (`pci.rs:108`), and 16 KiB of registers must be mapped without their neighbours |
| `userspace-drivers-spec.md` **stage 4** — the capability | not built | §4.4 |

**HDA is a better vehicle for stage 4's gates than the second `virtio-net-pci`
that spec proposes**, on three independent counts, and the spec's own §7.2 is
the reason for two of them:

1. **It is behind the vIOMMU with no guest-side negotiation.** §7.2's first
   vacuity trap is that QEMU hands a virtio device the bypassing address space
   unless `iommu_platform=on`, which additionally requires the guest to
   negotiate `VIRTIO_F_ACCESS_PLATFORM` — a flag `iommu-spec.md` §8.2 records
   this kernel's virtio drivers do not negotiate, leaving the whole green suite
   evidence for neither. `intel-hda` is an ordinary emulated PCI function, so it
   is translated by construction and there is nothing for a guest to decline.
   Measured today: it instantiates cleanly on `-machine q35,kernel-irqchip=split`
   with `-device intel-iommu,intremap=on,caching-mode=on,aw-bits=48`, QMP
   `query-status` returning `prelaunch`.
2. **Its interrupt capability is in config space, not in a mapped BAR.**
   `userspace-drivers-spec.md` §4.3 records that an MSI-X table lives inside a
   BAR and cannot be carved out at 2 MiB granularity, so a driver owns its own
   table. A device using plain MSI has its capability in config space, which
   §4.2 makes read-only — so the hole simply is not there. The residual
   `0xFEE00000` escape route remains and is I3's.
3. **It is the device the target machine actually has**, so stage 4's gates are
   exercised against the shape that has to work rather than against a second
   copy of a device nobody ships.

### 4.3 The kernel-resident interim, priced and declined

The thing §1 will not propose, priced so the owner can overrule it.

A kernel HDA driver behind the `trait Audio` that
`userspace-drivers-spec.md` stage 1 introduces would reuse everything already
built: `kernel/src/audio.rs` (158 lines) with its completion-record ring and
io_uring wiring, `DEVICE_AUDIO`'s claim, `AudioInfo`, `DmaPool`, `enable_msi`
(`pci.rs:174`), and soundd unchanged. What it would add is register definitions,
reset, CORB/RIRB, codec enumeration, the §2.3 traversal, stream setup and an
ISR — **an estimated 1,200–1,600 lines**, on the scale of `virtio_sound.rs`'s
649 plus the codec half that has no virtio counterpart. It needs **none** of
§4.2's chain.

Against it: every line is written to be deleted at
`userspace-drivers-spec.md` stage 7, it sets the precedent that spec exists to
break, and `wlan-plan.md` §9.9 has already ruled out the equivalent for WLAN
("not even temporarily, not even to get something working before the IOMMU
lands"). For it: it is the difference between a first note in days and a first
note behind I3, whose failure mode `iommu-spec.md` §12 describes as "nothing
works, which is the hardest kind to bisect".

CLAUDE.md says effort is never an argument and that an agent must not offer a
smaller deliverable because the owner is blocked. So this plan builds the right
thing and states the cost. **The trade is the owner's and it is a sequencing
trade, not a design one** — the end state is identical either way.

### 4.4 The ABI, item by item

`userspace-drivers-spec.md` §4.6 proposes eight syscalls. **HDA is the first
caller of all eight and needs no change to the shape of any of them.** What it
adds is five things, four of which add no ABI at all — which is the interesting
result.

| # | What | General or HDA-specific | Verdict |
|---|---|---|---|
| 1 | `SYS_PCI_LIST`, `SYS_DEVICE_CLAIM`, `SYS_DEVICE_CONFIG`, `SYS_DEVICE_BAR`, `SYS_DEVICE_BAR_MAP`, `SYS_DMA_MAP`, `SYS_DMA_UNMAP`, `SYS_DEVICE_IRQ` | **general** | unchanged; HDA is their first consumer |
| 2 | **A BAR mapping must be uncacheable** | **general** | `paging::CachePolicy` (`mm/paging.rs:41`) has two variants today, `DeferToMtrr` and `WriteCombining`, and `map_mmio` (`:595`) takes one. Neither is right for a *user* mapping of device registers: `DeferToMtrr` is PAT entry 0, which is WB and inherits whatever firmware's MTRRs decided (`iommu-spec.md` §12, task #139). **Adds no ABI**: a third `Uncacheable` variant, and `SYS_DEVICE_BAR_MAP` uses it unconditionally, because there is no BAR a driver should be allowed to map write-back. A parameter here would be a signature promising a choice that has one correct answer. |
| 3 | **The IRQ handle's records carry the interrupt-time timestamp and a count** | **general** | soundd's DLL exists to separate the device's period grid from scheduling latency, and it does that by timestamping *in the ISR* (`AudioCompletionRecord.timestamp_nanos`). A userland driver that timestamps at wake time folds the two back together and the DLL becomes a jitter amplifier. So a read on the IRQ handle returns `IrqRecord { count: u32, _pad: u32, timestamp_nanos: u64 }` — `AudioCompletionRecord` with `mask` replaced by `count`. **That is the completion record splitting along the trust boundary: the timestamp is the kernel's and the mask is the device's.** The mechanism to promote is `kernel/src/audio.rs`'s SPSC `RecordRing` made device-independent, **not `irq_ring`**: `irq_ring::isr_publish` coalesces a second IRQ into the existing record and **keeps the earlier timestamp** (`irq_ring.rs:57-70`), which is the right answer to "when was work first available" and the wrong one for a DLL, whose `update` measures against the *last* grid point of a batch (`soundd/src/main.rs:376`). So the 158 lines of `audio.rs` are not all deleted — its ring survives as the general mechanism. |
| 4 | **DMA mappings must be snooped** | **general**, and the HDA-specific alternative is the wrong answer | Intel HDA controllers carry a vendor-specific no-snoop control in PCI config space; a driver that leaves it as firmware set it may be doing DMA the CPU cache does not see. **The premise is read from in-tree driver behaviour and is not verified here against a datasheet — it is the risk, not the answer.** `userspace-drivers-spec.md` §4.2 has no config-write path and says a real need gets "a specific syscall with the offset named in the kernel", so HDA is the first concrete candidate for that escape hatch — **and it should be refused**, because the general answer is one layer down: VT-d's second-level PTE has a snoop-force bit gated by `ECAP.SC`, which QEMU's unit exposes as `snoop-control` (`iommu-spec.md` §8). Setting it in every mapping makes the device's own request irrelevant and makes the premise moot. **Adds no ABI.** `ECAP.SC` on the T14's unit is unread and is H0's. If it is clear there, the named-offset syscall comes back and this row is the record of why. |
| 5 | **`SYS_SET_RT_PRIORITY`'s gate moves** | **general** | It is gated at the dispatch site on the `DEVICE_AUDIO` claim, which is deleted here. It becomes a right on the device-claim handle, per `capability-handles-spec.md` §6.7 — the process init gave the audio device to may enter the RT band. **No new syscall; a changed check**, and one the code already says is too weak: "That is not yet spec §9.4's privilege gate: `SYS_OPEN_DEVICE` is first-come and ungated, so whoever wins the claim race gets the RT band with it" (`scheduler.rs:308-311`). |
| 6 | **Master volume and mute** | **not kernel ABI at all** | Three messages on soundd's existing control connection: `MSG_SET_MASTER_VOLUME { gain: f32 }`, `MSG_SET_MASTER_MUTE { on: bool }`, `MSG_GET_MASTER` → `MSG_MASTER_STATE { gain, muted }`. The tempting wrong answer is a syscall; volume is a mixer's business and the mixer is a userland process. |

**Deleted by this track**, once §6's H3 lands: `SYS_AUDIO_SUBMIT` (**71**),
`SYS_AUDIO_POLL` (**84**), `DEVICE_AUDIO` and its owner lock, `Descriptor::Audio`,
`AudioInfo`, `AudioCompletionRecord`, IDT vector `0x23`, `kernel/src/audio.rs`
(158 lines) and `kernel/src/drivers/virtio_sound.rs` (649 lines). **71 and 84
are retired, never reused** (CLAUDE.md). That deletion closes
`userspace-drivers-spec.md` stage 7 and is a third of its `virtio` grep.

**Rejected, named so the judgement is checkable:**

- `SYS_HDA_VERB` — a verb is a store to a ring the driver owns. A syscall for it
  would put a device protocol back in the syscall table, which is the defect
  `userspace-drivers-spec.md` §7.5 check 3 exists to catch.
- A kernel-side codec parser or widget-graph type. Same reason, one layer up.
- `SYS_AUDIO_SET_VOLUME`. §6 of the table.
- Keeping `DEVICE_AUDIO` as a device class "for compatibility". There is no
  compatibility to keep.
- **`RawKeyEvent` gaining a usage page.** It is `{ keycode: u8, modifiers: u8 }`
  (`toyos-abi/src/input.rs:25`) and carries a HID **Keyboard/Keypad page** usage
  with no page field. Media keys on a *USB* keyboard are Consumer-page and
  cannot be expressed. **The T14's own volume keys need no such change** — the
  Keyboard/Keypad page has Mute, Volume Up and Volume Down usages of its own, so
  §6's H8 is three table entries in `toyos-ps2` and no ABI at all. The USB case
  is filed, not fixed.

---

## 5. Testing

### 5.1 What QEMU can certify, and what it cannot

QEMU emulates HDA, so unlike WLAN this track has a harness arm. Measured today,
QEMU 11.0.3 on this host: two controller models (`intel-hda`, `ich9-intel-hda`)
and three codec models (`hda-output`, `hda-duplex`, `hda-micro`), each codec
taking `audiodev=` and `mixer=<bool>`. **`-audiodev wav` works exactly as it
does for virtio-sound**, so gate A's ground truth — the capture of what the
*device* received — transfers with no new instrument.

**Certifiable here:**

- Controller reset, `GCAP` decode, the refusals it can produce.
- Codec presence detection, CORB/RIRB, verb round trips.
- The graph traversal — **in its trivial case only**, see below.
- Stream descriptor setup, the cyclic BDL, `SDnLPIB`-derived completion masks,
  batching, the zero-on-complete rule of §2.4.
- The whole of gate A: tone continuity, dither presence, clicks, soundd's
  counters, the suspend/resume structure, and a distributional tier.
- Device *shape*: two controllers, and a controller whose codec offers no
  speaker pin (`hda-micro`).
- The refusal path end to end: a machine with a controller and no codec should
  fall back to the null sink and say why.

**Not certifiable here, and this is the honest part:**

- **The T14's widget graph.** QEMU's codec models present a handful of widgets
  and one path; a real codec has tens of widgets, mixers with several inputs,
  and pins whose configuration defaults are the only thing distinguishing a
  speaker from a header nobody soldered. **The traversal is the least-covered
  and highest-risk code in the plan**, and §5.2 is the mitigation.
- Jack detection behaviour on a codec that models it (QEMU's may report a pin as
  permanently present; unmeasured).
- The vendor no-snoop bit, `ECAP.SC` on a real unit, BAR relocation on a packed
  real bus, and whether `00:1f.3` offers MSI.
- Whether the T14's speakers are reachable over the HDA link at all (§3).
- **Anything about cost.** TCG's distortion is non-uniform (CLAUDE.md records
  1.06×–6.5× by operation), so CLAUDE.md's 2× bar is answerable only on the T14
  or under KVM, same session, A/B.

### 5.2 The trick that closes most of the graph-traversal gap

**Make the traversal a pure function, and make the T14's own graph a fixture.**

`toyos-hda` is a `no_std` crate with no I/O: verb encoding and decoding, the
codec graph model, `find_output_path`, `SDnFMT` encoding, BDL construction. It
is host-tested the way `toyos-sched`, `toyos-gpt`, `toyos-fat32` and
`toyos-ps2` are — `cargo test` inside the crate, seconds, no guest.

Its fixtures are three: a synthetic graph; QEMU's codec dumped from a boot; and
**the T14's codec dumped by H0 and committed**. From H0 onward, the hardest part
of this driver is verified in the harness against the real machine's own answer.
That is `wlan-plan.md` §5.1's L1 move applied where it pays most, and it is the
single most valuable structural decision in this section.

State-space attacks, listed separately from teeth because
`specs/metal-track-history.md`'s lesson is that mutating your implementation
tests the paths you wrote and never the states you did not construct:

- A connection list that cycles, and one that points outside the function
  group's node range.
- A widget claiming more subordinate nodes than the group has.
- A codec answering all-ones to every verb (an absent or wedged codec).
- A RIRB response whose codec address is not the one asked.
- A graph with **two** function groups, one of them a display codec with a
  perfectly valid output path and no speaker (§2.3's trap, as a test).
- A pin whose configuration default says "no physical connection".
- A graph with no output converter reachable from any speaker pin.

### 5.3 Gate A with two backends

**The recorded sample describes virtio-sound and cannot be reused.** A second
backend is a different device with a different period grid, different completion
batching and possibly a different rate. Concretely:

1. **Four new sections in `tests/audio-baseline.toml`** — `audio_tone_hda.smp1`,
   `.smp8`, `audio_tone_hda_load.smp1`, `.smp8` — recorded on their own
   30-invocation session by that file's own protocol. **The existing four are
   not touched**, and re-recording them is not licensed by this track.
2. **The instrument's physical scale becomes per-config.** `PERIOD_SECS =
   128.0/44100.0` (`tests/common/audio.rs:63`) and `PIPELINE_DEPTH_US = 8 ×
   PERIOD_SECS` (`:293`) are global constants today. If QEMU's HDA codec does
   not offer 44,100 Hz — unmeasured — the HDA arm runs at another rate and every
   ceiling derived from those constants is computed against the wrong scale.
   This is exactly the shape of the four instrument defects
   `specs/audio-gate-history.md` records, so it is named before it is built.
3. **`check_suspend_structure` keeps working, and one of its neighbours rots.**
   The `soundd: suspended` marker is backend-independent. The comment in
   soundd's mix loop ordering `soundd: resumed` before "the kernel's own
   `virtio-sound: stream 0 started` line" becomes false at H3, when there is no
   kernel line.
4. **The suite pays for it.** The fast tier gains four audio configs at roughly
   a boot each, in the serial audio block. The thorough tier at N=30 goes from
   four configs to eight — **from ~17 minutes to an estimated ~34**. Mitigation:
   the thorough tier takes a backend filter, so a scheduler-migration transition
   gates on the virtio arm alone, which is what it has always meant, and the HDA
   arm runs when HDA changes.
5. **The analyzer gains a phase-continuity check** (§2.4). It is the only
   instrument that would catch a repeated period, and the zero-on-complete rule
   is the only reason the existing one stays valid. Two independent guards on
   the same property is the right number when one of them is a design promise.

### 5.4 How a metal-only failure gets caught

It does not get caught by the harness. That is the honest sentence and
everything below is mitigation rather than coverage.

- **Push correctness downward.** §5.2's crate, plus §6's H3 measuring the
  userspace-driver boundary against a backend the harness fully certifies. When
  the T14 fails, the failure is in the T14's codec or in the controller's
  behaviour — not in the graph walk, the verb encoding, the boundary, or soundd.
- **The driver's decisions are a log.** One structured line per decision:
  codecs found, function groups, the chosen codec and why, the chosen pin with
  its configuration default decoded, the converter, the format, the amplifier
  settings. It lands in `/log/kernel.log` on the stick, which macOS auto-mounts
  (`specs/metal-log-capture.md`). A report of "no sound" is then a diff against
  a working boot rather than a debugging session.
- **soundd's counters are printed on metal too.** So a *rate* question on the
  T14 is answerable — by comparing the T14 against itself across builds, in one
  session. **It is never answerable against the recorded QEMU sample**: a
  different machine's distribution is not a baseline, and CLAUDE.md's
  same-session rule is the whole of that argument.
- **The owner listens.** That is the T14's arm of gate A and the spec should say
  so rather than imply an instrument exists. What to listen for: the same 440 Hz
  tone the harness plays, then doom.
- **The thing that would make metal audio self-verifying is capture**, through a
  codec loopback path from the converter back to an input converter. It is out
  of scope (§2.5, §8) and is named here so nobody rediscovers the idea and
  assumes it was missed.

---

## 6. Stages

Every stage leaves the tree green: `cargo run -- --build-only` clean and
`cargo test` green including gate A's fast tier. Sizes are **estimates**
throughout.

| Stage | Content | Size (est.) | Gate |
|---|---|---|---|
| **H0** | **Feasibility, on metal. No driver.** A kernel diagnostic behind a feature flag, present only in the diagnostic image, deleted at H9. It carries the comment `specs/device-test-strategy.md` requires of a kernel-feature actuator, and the reason nothing else can reach it is exact: **there is no way for a userland process to touch a codec before the capability of §4 exists, and the questions it answers are the ones that decide whether that capability will ever be given this device.** Two halves on one boot. **(a) Handoff**, for `00:1f.3`: its DMAR device scope, whether its isolation scope is a singleton given four sibling functions (§7 risk 1), whether it carries an RMRR, whether it offers MSI or MSI-X and how many vectors, BAR0's size/width/prefetchability and whether a 2 MiB relocation target exists, and the unit's `ECAP.SC` (§4.4 item 4). **(b) Codec**, using the immediate-command registers so it needs no DMA and no capability: release `GCTL.CRST`, read `GCAP`/`VMIN`/`VMAJ` and `STATESTS`, and for every codec present dump vendor and device id, every function group, every widget's capabilities and every pin's configuration default. Plus: log every i8042 scancode sequence that decodes to no key, and press the three volume keys. | ~120 + ~250 lines kernel | The log carries a named line per item, read off the panel and off `/log/kernel.log`. **If the isolation scope or an RMRR refuses the device, or `STATESTS` reads zero, this track stops here and is re-decided.** |
| **H1** | **`toyos-hda`, the host-tested core.** Verb encode/decode, the graph model, `find_output_path`, `SDnFMT` encoding, BDL construction. No I/O. | ~1,500 lines Rust | `cargo test` in-crate, against three fixtures including **H0's dump of the T14's own codec**. §5.2's state-space attacks, each with teeth: deleting the cycle check must red the cyclic fixture, and deleting the speaker-pin preference must red the two-codec one. |
| **H2** | **Prerequisites land.** Not HDA work: `iommu-spec.md` I3 and I4, `userspace-drivers-spec.md` stages 3 and 4. Shared with `wlan-plan.md` W5. **Proposes that stage 4's capability be staged against a second `intel-hda` rather than a second `virtio-net-pci`** (§4.2). | — | Those specs' own exit criteria |
| **H3** | **The userspace audio backend seam, with virtio-sound as its first implementation.** soundd grows a backend trait and drives virtio-sound *from userland* through §4.4's capability. Deleted: `virtio_sound.rs`, `audio.rs`, `SYS_AUDIO_SUBMIT`, `SYS_AUDIO_POLL`, `DEVICE_AUDIO`, `AudioInfo`, `AudioCompletionRecord`, vector `0x23`. Closes `userspace-drivers-spec.md` stage 7. | ~1,200 lines Rust moved, **807 lines of kernel deleted** | **Gate A's thorough tier, `cargo test --test toyos-build -- --audio-gate 30`, same-session A/B against the pre-stage tree.** Same rule as a scheduler-migration transition and for the same reason. **This is the stage that can revert the direction, and it is deliberately before HDA exists.** |
| **H4** | **HDA behind the same seam, in QEMU.** Reset, CORB/RIRB, enumeration, path selection from H1, stream + cyclic BDL, `SDnLPIB`-derived masks, zero-on-complete. New profiles: one HDA machine, one with two controllers, one whose codec has no speaker pin, one with a controller and no codec. | ~2,000 lines Rust | New gate-A arms (§5.3) with their own four baseline sections; `cargo test -- hda` for the shape configs. Teeth: removing the zero-on-complete write must red the phase-continuity check. |
| **H5** | **The T14: first note.** Flash, boot, listen. The enumeration trace lands in `/log/kernel.log`; the codec dump becomes an H1 fixture. | — | **L4, metal only.** The owner hears the 440 Hz tone from the speakers. The run's soundd counters are recorded as the T14's own first baseline. |
| **H6** | **Jack detection and output routing.** Polled pin sense on soundd's existing 2 s cadence; route between the speaker and headphone pins; ramp master gain to zero across the switch so it does not click. | ~300 lines Rust | Metal: headphones in, sound follows within one poll interval, and back out again. Harness if QEMU's codec models pin sense; if it does not, the switch logic is host-tested in `toyos-hda` and the *transition* is asserted in QEMU by driving it from a test hook. |
| **H7** | **Master volume and mute.** A gain on soundd's mix bus, §4.4 item 6's three messages, the existing ramp machinery, and `toybox audio` to read and set it. | ~400 lines Rust | **Harness, with real teeth**: a guest test sets master to 0.5 and the wav capture's amplitude halves; sets mute and the capture goes silent; neither transition fires the click detector. |
| **H8** | **The volume keys.** Three `SET1_E0` entries in `toyos-ps2` — Keyboard/Keypad-page Mute, Volume Up and Volume Down usages, against what H0 observed the EC actually sends — and the surface owners consuming those three usages instead of forwarding them (§6.1). | ~150 lines Rust | Harness: QMP injects the scancodes and a guest test asserts soundd's master state moved and the capture followed. Metal: the owner presses F1/F2/F3. |
| **H9** | **Persistence, and H0's probe deleted.** Master volume and mute survive a reboot. **Blocked** — §6.2. | ~200 lines Rust | Reboot in the harness and read the value back; on metal, only once there is a volume to write to. |
| **H10** | **The end condition.** `userspace-drivers-spec.md` §7.5's checks pass for audio; CLAUDE.md's architecture and `metal-hardware-inventory.md`'s undriven list updated; `known-issues.md` entries closed. | — | Those commands |

H0 and H1 are independent of everything else and of each other, and H0 is a
diagnostic on a boot the owner is doing anyway. **H0 should be run early and
cheaply, because it is the only thing here that can invalidate the plan rather
than merely cost time.**

### 6.1 Where the volume keys go, and the exception they force

CLAUDE.md's input model is that events flow *down* the tree: the compositor
forwards whole transitions to the focused window and translates nothing. A
volume key delivered that way reaches the focused application, which is wrong —
every application would have to handle it.

**So the three usages are consumed by the surface owner and not forwarded.** The
compositor, `/bin/console` and `/bin/terminal` each do it, through one SDK
helper so each site is three lines. It is a stated exception to "events flow
down", it is what a machine-global hotkey means, and naming it is cheaper than
discovering it.

`toyos-keymap::Translator` needs no change: the three usages have no character
and produce nothing.

### 6.2 Why persistence is blocked, and on what

§3: **the T14 has no writable persistent filesystem.** `/home` is a tmpfs
because the NVMe disk is deliberately refused, `/boot` answers
`PermissionDenied` to userland, and `/log` is the log's volume.

`/log` would work today, and this plan **recommends against it**. CLAUDE.md's
rule is that a mount is named for its role; `/log` is the log's role, and
putting configuration there is naming by convenience, one step from selecting a
volume by what happens to be writable.

The unblocking work is `specs/introspection-plan.md` W7 — the format capability
and adopt-by-witness — which is gated on the owner's review of its §4. H9 waits
on that, and until then master volume resets to its default on every boot on the
T14. In the harness it persists, because the harness has a disk.

### 6.3 H0 is built. How to run it, and how to read what comes back

**Built**, behind the kernel feature `hda-probe`
(`kernel/src/drivers/hda_probe.rs`, ~640 lines). It runs last in the peripheral
phase, over **every** class-0403 function the PCI walk returned rather than the
first, does no DMA at all — the verbs go through the immediate-command
registers — and bounds every wait. Deleted at H9 with the feature.

The harness arm is `cargo test -- hda_probe` on `Profile::MetalHda`, a machine
built so one boot runs both arms of each question below (§5.1 lists what that
does and does not certify). It also boots the same machine with a plain kernel
and requires no `hda:` line at all, which is the only assertion that binds
"nothing in the ordinary boot path takes that controller out of reset".

#### Running it on the T14

```
cargo run -- --diag-boot --build-only          # target/bootable-diag.img
```

**The one thing that is not wired**: `src/build.rs` hands the kernel no
features on the `--diag-boot` path, and that file was another agent's ground
for the whole of H0's implementation. Until one line there adds `hda-probe` to
`Boot::Diag`, the flashed image carries the feature only if the kernel is built
with it by hand. This is the sole item between H0 and the T14 and it is
recorded in `specs/known-issues.md`.

Flash, boot, and — **before pressing any other key** — press Mute, then Volume
Down, then Volume Up. Then read `/log/kernel.log` off the stick on the Mac.
Everything the probe says is a line beginning `hda:`, and the four verdict
lines are `hda: (a)`, `(b)`, `(c)`, `(d)`.

#### (a) Handoff — what each answer means

| Line | What it means for the plan |
|---|---|
| `(a) scope members=1 … a singleton` | `iommu-spec.md` §7.3 permits handoff on this count. Unblocked. |
| `(a) scope members=N … not a singleton` | **The expected answer on this machine**, since `00:1f.3` is one of five functions. §7.3 refuses the device *as the rule is written*, and it refuses `00:1f.6` — gate N's metal target — with it. This does **not** end the track; it moves the decision into `iommu-spec.md`, because the rule was written for peer-to-peer behind a switch and a root-complex-integrated function is not that. `hda: scope upstream-bridge none` on the same boot is the evidence: there is no switch, so there is no port whose ACS could be checked, and what two functions of one root-complex device can do to each other is not reported by any register on the bus. **Owner's call**, and it is `iommu-spec.md` §7.3's to restate — not this file's. Until it is restated, both audio and networking on the T14 are blocked behind it, and that is the single most consequential thing H0 can return. |
| `(a) rmrr none` | Nothing to resolve. |
| `(a) rmrr <range>` | §7.4 refuses the device for userspace handoff outright, and unlike the scope rule this one has a hard reason: identity-mapping firmware's range into an untrusted driver's domain hands that driver memory it was never given. **This one really does end §1's answer for this device** — either HDA stays kernel-resident (which §8 item 2 forbids) or the T14 has no audio. Escalate rather than work around. |
| `(a) msi vectors=N msix=none` | The expected and the *preferred* answer (§4.2 item 2): the capability is in config space, so there is no MSI-X table inside a BAR to carve out. |
| `(a) msi=none msix=none` | `userspace-drivers-spec.md` §4.5 makes the function ineligible. No known real part is built this way; if the T14 says it, suspect the probe before the silicon. |
| `(a) bar0 … movable=y 2m-page-neighbours=0` | `userspace-drivers-spec.md` stage 3's 2 MiB re-assignment has somewhere to go. |
| `(a) bar0 … 2m-page-neighbours=N` (N>0) | The BAR shares its 2 MiB page with the named functions' registers, so it cannot be handed over as it sits. Stage 3's relocation is then load-bearing rather than tidy, and `movable=y` is what says it is possible at all. |
| `(a) unit=N … ecap.sc=y` | §4.4 item 4's general answer holds: snoop-force in every mapping, no config-space write path, and the vendor no-snoop control is moot. |
| `(a) … ecap.sc=n` | Risk 6 realised. The named-offset config write comes back as a specific proposal, and §4.4 item 4 is the record of why it was refused first. |

#### (b) Is a codec on the link — the answer that can end the track

| Line | What it means |
|---|---|
| `(b) statests=0x…` non-zero | The legacy link carries a codec. H1 through H10 proceed. |
| `(b) statests=0x0000 — NO CODEC ON THE LEGACY LINK` | **The track is over on this machine and no driver work changes it.** Tiger Lake ships in configurations where the codec is on SoundWire, or reachable only through the Smart Sound DSP with vendor firmware — §8 item 4 puts both out of scope, and §2.5's last row said the risk out loud. What replaces this plan is a SoundWire or SOF-shaped effort an order of magnitude larger, and that is the owner's decision, not a next stage. |
| `hda: (b) GCAP reads all ones` | The register window answers nothing: the function is powered down past what the probe's D0 transition reached, or hidden by firmware. Not a verdict about codecs — go back to firmware setup before concluding anything. |
| `hda: codecN the controller did not answer an immediate command` | `STATESTS` named a codec and the controller has no working immediate-command interface. H0's dump needs CORB/RIRB on this part; nothing above the verb layer is invalidated. |

**This generalises past this laptop.** `STATESTS` reading zero on a controller
that resets cleanly is the signature of the whole 2019-onward Intel line where
audio moved behind the DSP. Any future ToyOS machine of that vintage gets the
same answer, so the codec-presence read belongs in whatever eventually replaces
this probe rather than being treated as a T14 quirk.

#### (c) The widget dump, and its second life

Everything between `hda: codecN vendor=` and the next controller's banner is the
fixture. Each line is `key=value` after an `hda:` prefix, carries the codec's
own raw word beside every decoded name, and describes exactly one node — so a
host-side reader is a line split and nothing more. §5.2 makes this the fixture
`toyos-hda`'s `find_output_path` is tested against, which is the only way the
least-covered code in the plan gets verified against a real machine.

`hda: (c) pins reporting a speaker default device: 0` is the interesting
failure: the codec answered, the graph is there, and no pin claims a speaker.
§2.3 then has nothing to choose and the correct behaviour is a refusal naming
every codec found — never a fallback to line-out, which is how a laptop ends up
playing through a header nobody soldered.

#### (d) How many codecs

`hda: (d) codecs=1` is one machine's answer and licenses nothing: §2.3's rule is
*enumerate all, choose by capability, refuse by name*, and it does not relax
because this laptop happens to have one. `codecs=2` or more is the Iris Xe's
display audio sitting beside the analogue codec, and the probe prints the
first-match warning with it.

#### The volume keys

No new mechanism: the i8042 driver already names every byte run that decoded to
nothing, and under `hda-probe` its list holds 24 bytes instead of 8 so three
keys' make and break fit in one boot. Read the `i8042:` lines:

- `no event from [0xe0, 0x20, …]` — the keys reach the i8042 and decode to
  nothing, and those bytes are exactly what H8's three `toyos-ps2` entries are
  written against.
- `keys` in the counter line went up by three and nothing was listed — the EC's
  codes already map to something, which means today they *type* rather than
  change the volume, and H8 is a change to what the surface owner does with
  three usages it already receives.
- The `bytes` counter did not move at all — **the volume keys are not on the
  i8042**, they are an ACPI or WMI event, and H8's "three table entries and no
  ABI at all" premise is wrong. That is a finding for §4.4's rejected list, not
  a small fix.

---

## 7. Open risks

Each with what settles it, and how early.

1. **`00:1f.3` may not be handoff-able at all, and the same question refuses the
   I219.** `iommu-spec.md` §7.3 hands a device to userspace only if its
   isolation scope is a singleton, and names multi-function devices as one of
   the two ways that fails. The T14's HDA is **function 3 of a five-function
   device** whose other members are the eSPI bridge, SMBus, the SPI flash
   controller and the Ethernet NIC (§3). If ToyOS's scope computation refuses
   it, it refuses `00:1f.6` too — **which is gate N's metal target
   (`wlan-plan.md` §7)** — and both tracks lose their device on that machine.
   The rule would then need restating in terms of what a root-complex-integrated
   function can actually reach, which is a decision about the IOMMU spec and not
   about this one. **Settled by H0 on one boot**, and it is the reason H0 exists.
2. **The codec may not be on the HDA link.** §3. `STATESTS` reading zero ends
   this plan on this machine and no driver work changes it. **Settled by H0's
   second line.**
3. **The userspace-driver boundary may cost more than audio can pay.**
   `userspace-drivers-spec.md` §9 already predicts stage 7 is the one most
   likely to be reverted, and gate A's thorough tier at N=30 does not detect a
   doubling of the dropout rate. §4.1 argues the path gets *shorter*, not
   longer, and that argument is unmeasured. **Settled by H3, deliberately before
   HDA exists**, and the honest failure mode is that H3 goes green while
   something got worse below its resolution.
4. **An RMRR on `00:1f.3`.** `iommu-spec.md` §7.4 refuses a device carrying one,
   QEMU publishes none, and the T14 is the first machine that can answer. H0.
5. **MSI, and BAR relocatability.** §4.2 and `userspace-drivers-spec.md` §4.3
   both assume answers nobody has read. H0.
6. **`ECAP.SC` may be clear on the T14's unit**, in which case §4.4 item 4's
   general answer is unavailable and the HDA-specific config write comes back.
   H0.
7. **Gate A's instrument cannot see a repeated period.** §2.4's zero-on-complete
   rule is what keeps the existing gap detector valid across both backends, and
   it is a design promise rather than a measurement. If it is wrong, the gate
   goes green on audible harm. §5.3 item 5 is the second guard; build it.
8. **QEMU's codec may not offer 44,100 Hz.** Unmeasured. It changes the HDA
   arm's physical scale and every ceiling derived from it (§5.3 item 2).
9. **The graph traversal is the least-covered code in the plan**, and its
   fixture coverage is one real machine's codec. A second machine would answer a
   different question; there is not one.
10. **Nothing here is measurable against CLAUDE.md's 2× bar.** §5.1.

---

## 8. Explicitly not doing

1. **Capture.** No microphone, no ADC path, no input pins. It also removes the
   only route to a self-verifying metal boot (§5.4), and that is the cost.
2. **A kernel-resident HDA driver**, at any point, even temporarily. §4.3 prices
   it for the owner and declines it.
3. **A separate HDA daemon.** §4.1: the driver is a library in soundd.
4. **The DSP / Smart Sound block**, vendor topologies, and firmware loading for
   any of it.
5. **HDMI/DisplayPort audio**, and any codec that is not the one with a speaker
   pin. The others are enumerated and named, never silently skipped.
6. **Unsolicited responses.** §2.5 — polling replaces them and costs latency,
   not function.
7. **Codec power management** beyond D0 on the chosen path.
8. **The DMA position buffer**, and with it Linux's whole `position_fix` quirk
   family. §2.4.
9. **A codec quirk table.** One machine, one codec, one path chosen from the
   graph the codec itself reports. A table of per-machine fixups is the legacy
   this project does not have.
10. **A generic "audio HAL"**, an audio framework, or an abstraction over
    backends beyond the one trait H3 introduces with two implementations. A
    framework before there are three is an abstraction with no evidence behind
    it (`userspace-drivers-spec.md` §8.7).
11. **Master-volume access control.** There is no mechanism today by which
    soundd can tell an authorized volume client from any other process — service
    names are not gated. H7 ships it open and files that, which is the same hole
    `SYS_OPEN_DEVICE`'s first-come claim already has (`known-issues.md` §1).
    Closing it is `capability-handles-spec.md`'s work.
12. **A codec amplifier as the master volume control.** §9.

---

## 9. Where I think this is wrong

A plan that cannot say no is not a plan.

- **H3 re-orders another spec's stages, and this spec is not entitled to.**
  `userspace-drivers-spec.md` §6 runs net → gpu → sound; this plan puts sound
  first. The argument is that audio is the only subsystem with a distributional
  gate that can measure the boundary cost, so the stage most likely to be
  reverted should be the one that runs while three drivers do not yet depend on
  the capability. That is a real argument and it is still a change to somebody
  else's plan, made in a file about a different device. **The owner should
  accept or reject it explicitly.**
- **The master volume is a digital gain in soundd, not a codec amplifier, and
  that is a choice with a measurable downside.** A digital gain of G costs
  20·log₁₀(G) dB of signal-to-noise against a fixed dither floor: at −30 dB
  master, 16-bit's headroom is spent down to about 63 dB. The reasons to take it
  anyway are that it works identically on every backend including the null sink,
  it reuses a ramp that already exists and is already glitch-free, and a codec
  amplifier's step size, taper and mute semantics are per-codec decoding work.
  **It is reversible, and the thing that would reverse it is the owner hearing
  the noise floor.**
- **The 2 s jack poll is a number I chose because soundd already wakes on it.**
  It is policy, not physics, in exactly the sense `refill_floor_nanos` is. Two
  seconds between plugging headphones in and hearing them may be too long.
- **"One instrument certifies both backends" rests on the zero-on-complete rule
  being right**, and that rule is an argument in this file rather than a
  measurement. Risk 7.
- **§5.2's fixture is one machine's codec.** It makes the traversal harness-
  verified against the machine that matters and says nothing about the next one.
  That is worth a great deal and it is not general coverage, and the difference
  should not be blurred.
- **H1 is sized at ~1,500 lines with nothing to measure against.** Unlike
  `wlan-plan.md`, whose estimates restate counted C, there is no reference
  implementation this project is licensed to count. Every size in §6 is a guess
  and should be read as one.

---

## 10. Corrections to existing records

Recorded, not fixed, per this task's scope.

1. **`specs/metal-hardware-inventory.md:507-508` prices an HDA driver as
   "CORB/RIRB, stream descriptors, codec enumeration".** True, and incomplete in
   the way that matters: it omits the widget-graph traversal, which §5.1 argues
   is the least-covered and highest-risk part, and it omits that the *codec* is
   invisible to the boot log that entry was derived from — so the entry cannot
   say whether the machine has one.
2. **`specs/userspace-drivers-spec.md` §6 stage 4 stages the capability against
   a second `virtio-net-pci`.** §4.2 here gives three reasons an `intel-hda`
   is a strictly better vehicle, two of which are that spec's own §7.2 vacuity
   traps not applying to a non-virtio device using plain MSI.
3. **`specs/iommu-spec.md` §8 and `specs/userspace-drivers-spec.md` §7.2 record
   QEMU 11.0.2, measured 2026-08-02.** This host now reports **11.0.3**
   (`qemu-system-x86_64 --version`, 2026-08-04). No measurement in either file
   is known to have changed; the version in them is simply no longer this host's.
4. **`tests/common/audio.rs`'s `PERIOD_SECS` and `PIPELINE_DEPTH_US` are global
   constants** derived from one backend's 128-frame period at 44,100 Hz and
   8-buffer pipeline. A second backend makes them per-config, and until they are,
   any HDA arm's ceilings are computed against the wrong scale (§5.3 item 2).
5. **soundd's mix loop carries a comment ordering `soundd: resumed` before "the
   kernel's own `virtio-sound: stream 0 started` line".** H3 deletes that kernel
   line, and the comment with it.
6. **soundd's suspend block anticipates exactly this device**: "The one event
   that makes grace nonzero is a hardware backend that pops on stop, advertised
   per-backend through `AudioInfo`." HDA stopping a stream descriptor does not
   power down the codec or its amplifiers, so a pop is not expected — but that
   is a hardware property, it is H5's to listen for, and `AudioInfo` is deleted
   at H3, so the hook that comment names has to move with the backend trait.
