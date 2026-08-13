# ToyOS HDA audio — the Intel High Definition Audio plan

The one audio device the one real machine has: `00:1f.3`, class `0403`,
`8086:a0c8`, prog_if `0x80`, "500 Series Chipset Family On-Package High
Definition Audio", listed **undriven** in `specs/reference/metal-hardware-inventory.md:121`
and read off a boot that happened. **On metal today there is no audio at all**
— soundd prints `no audio device, presenting a null sink … streams discarded`
(`userland/soundd/src/main.rs:1548`), and none of gate A's guarantees describe
that machine.

This file is the shape of the whole track. It is a spec and nothing in it is
built. It is also the first device of `specs/plans/userspace-drivers-spec.md`, so its
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

**Does HDA go in the kernel or in userland?** Userland, inside soundd — and the
boundary between them is **who writes an address**. soundd maps the whole BAR
**read-only**, reads it directly, and reaches every register *write* through the
kernel; the kernel never accepts a physical address from soundd, only a handle
to a buffer it allocated itself. That is the **kernel stub**, decided by the
owner on 2026-08-06 against both a fully-mapped userspace driver and a kernel
driver, and §4.1 is its whole statement.

`specs/plans/userspace-drivers-spec.md` §3.1's criterion — *a driver stays in the
kernel only if the kernel needs it while userspace is dead* — does not keep
audio under any reading, and the stub does not violate it: the codec
enumeration, the widget-graph traversal, the path choice, the format, the amps
and the mixer are all soundd's, and what is left in the kernel writes no policy
down. `specs/plans/wlan-plan.md` §3.1's rule against a kernel-resident interim driver
binds here and is met — §4.3 is the argument, and §8 item 2 still forbids the
thing it names.

**What that answer costs is now small, and this is the point of it.** The first
draft of this file put interrupt remapping (I3), domains and mapping (I4), BAR
sizing and relocation (`userspace-drivers-spec.md` stage 3) and the capability
itself (stage 4) ahead of the first HDA register — I3 being the step
`specs/iommu-spec.md` §11 calls the one that "can black-screen the machine".
**The stub needs none of them for containment**, because containment no longer
rests on translating a driver's DMA: the device is only ever programmed with
addresses the kernel chose. §4.2 is the re-derived chain and it is four rows
shorter.

**What this spec cannot settle** is the residual the stub does not close: soundd
reads a 2 MiB page it does not fully own (§4.1's read-side residual), and
nothing here measures HDA against CLAUDE.md's 2× bar (§5.1).

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
a way that matters more than the saving.

**Counted, because §4.1's kernel stub is priced on it: a period costs zero
register writes from the driver's own logic, and at most one from the ISR.**
The engine cycles the BDL unaided; `toyos-hda`'s `stream.rs` builds that ring
once. What the driver does per period is an `SDnLPIB` **read**, 512 bytes of
zeroing stores and 512 bytes of PCM stores — no register write of any kind. The
one write is `SDnSTS`'s write-1-to-clear interrupt acknowledgement, and it is
**fewer than one per period** because an interrupt covers one or more buffers.
It is also unambiguously the kernel's: an interrupt acknowledged from userland
is an interrupt left asserted across a scheduling round trip. Every BDL entry
this crate builds sets `interrupt_on_completion` (`toyos-hda/src/stream.rs:87`),
so at rest that is one ISR write per period and under batching fewer.

**Under the stub that is the whole answer**: the per-period syscall count is
zero, because the only per-period write is on the side of the boundary that
owns the IDT already. §4.1 states what the reads cost and what the writes that
are *not* per-period cost.

The underrun semantics:

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
2. Teach the analyzer to detect a repeat. `specs/assessments/audio-gate-history.md` records
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

**What zero-on-complete does not buy, found on the T14 (`specs/issues/audio/`).** It
makes an unfilled period *sound* the same on both backends. It does not make one
*mean* the same, and soundd's free list was written for the meaning virtio has:
a period soundd has not submitted is a period the device does not have. Here the
engine owns every period for as long as it runs, so a period soundd holds back
is played anyway **and completed a second time** — which is the assertion above
firing, and it is right to. Three rules of the mix loop rested on the queue
meaning: §5.10's deferral (bounded by unplayed audio, not by the engine's
return), §5.8's drain-by-not-submitting (exactly one lap, no margin), and taking
the lowest free index first (the engine plays the ring's order, so a batch that
wraps is filled backwards — a splice with no silence in it for the gap detector
to see). `Backend::pipeline` names the difference and those three sites ask it;
`stream::decode` reads the driver's position back off every mask rather than
having it stepped and hoped for — a mask read late is the OR of several
`completed` calls, so it can name a whole lap, and the driver is told that in
those words rather than handed a position the mask does not carry. Gate:
`hda_client_stall`, whose two arms must answer differently — the ring reporting
underruns and holding nothing, the queue deferring — so that neither deleting
the deferral nor letting the ring hold a period goes green.

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

**Shown**, from `specs/reference/metal-hardware-inventory.md` and the verbatim boot log in
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
  learn and it is H0's first line.** It is also the *only* thing that decides
  whether the T14 gets audio cheaply: the machine's four internal USB devices
  are the fingerprint reader, the camera, the smartcard reader and the boot
  stick (`metal-hardware-inventory.md:223`, `:484`) — **no USB audio device** —
  and the headphone jack is a pin on whatever answers rather than a second path.
  §6.3's (b) block prices both outcomes.
- **MSI.** `specs/plans/userspace-drivers-spec.md` §4.5 makes a function offering
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

### 4.1 One process, and the line through it is who writes an address

**The HDA driver is a library crate linked into soundd, not a separate daemon.**

CLAUDE.md's architecture line is already this: "compositor, netd, soundd, sshd.
Each claims a device from the kernel, maps hardware buffers into its own memory,
and serves clients over IPC." soundd claiming `00:1f.3` rather than
`DEVICE_AUDIO` is that sentence working, not an exception to it.

The argument that decides *that* is the period budget. Today one period costs
soundd a wake plus **2k + 7 syscalls per wake**, where k is the periods refilled
in the cycle — one `SYS_IO_URING_ENTER`, one `SYS_READ_NONBLOCK` of the
completion records, one `SYS_WRITE_NONBLOCK` per client, four `SYS_CLOCK`, and
then one `SYS_CLOCK` and one `SYS_AUDIO_SUBMIT` per period. `SYS_AUDIO_POLL` is
not among them; it has no dispatch arm and no caller, and
`capability-handles-spec.md:759` already calls it dead ABI (§10 item 7). With
the driver in soundd, the completion mask comes from an `SDnLPIB` read in
soundd's own address space and there is no submit at all (§2.4). A *separate*
driver daemon would put an IPC round trip inside a 2.902 ms period, which is the
shape `specs/plans/userspace-drivers-spec.md` §9 predicts will get stage 7 reverted.

So the boundary move makes the audio path **shorter**, not longer. That is worth
stating plainly because the opposite is the intuition, and §4.1.2 is where the
stub is checked against it rather than assumed to preserve it.

#### 4.1.1 The stub

Decided by the owner on 2026-08-06, against a fully-mapped userspace driver and
against a kernel driver. **The line is who writes an address.**

1. **soundd maps the whole BAR read-only.** ToyOS enforces `mmap` prot — no
   `WRITE` is a read-only PDE — and the same is true one layer down:
   `AddressSpace::map_range` sets `PAGE_WRITE` only when its `writable`
   argument is true (`kernel/src/mm/paging.rs:333-336`). The BAR cannot be split
   by paging in any case: `SDnLPIB` and `SDnBDPL` are 20 bytes apart and this
   kernel maps 2 MiB pages.
2. **Reads stay direct.** §2.4's per-period `SDnLPIB` read is one MMIO read in
   soundd's own address space and costs no syscall. That is the whole of §4.1's
   argument and the stub keeps it.
3. **Every write goes through the kernel** — not only the address-bearing ones,
   because the BAR cannot be split. The kernel never accepts a physical address;
   it accepts a handle to a buffer it allocated and programs the register
   itself.
4. **Zero copies.** The PCM buffer is kernel-allocated, mapped **writable** into
   soundd, and the BDL points at it. soundd writes samples straight into the
   buffer the hardware reads — the arrangement `virtio_sound.rs:562` already has
   with `shared_memory::register`.
5. **Interrupts are the kernel's by construction** (it owns the IDT and MSI), so
   §2.4's `SDnSTS` acknowledgement is kernel-side already and is not a new cost.

**Why the read-only mapping is the load-bearing decision and not the register
allow-list.** A boundary built on enumerating the dangerous registers fails
silently the day the enumeration is wrong — a vendor register nobody read the
datasheet for, a second BAR nobody knew about. A boundary built on *no writes at
all* does not: it is closed by construction and the enumeration becomes a
convenience rather than a proof. That is the reason to take this shape over
"map it writable and police the address registers", and it is the answer to
§4.1.3's list too.

**So the kernel's write surface is a positive list, never a refusal list.** The
kernel stub carries the offsets soundd may ask it to write; anything else is
refused **by name**. The polarity is the point — an allow-list that is missing
an entry costs a driver that cannot bring a stream up and says so, and a
refusal-list that is missing an entry costs a device pointed at kernel memory.
Every entry on the list is justified by the same one-line property: *its value
is not an address*. §4.1.3 is that list and its complement.

**What is not on it, because the kernel writes it unasked.** Controller reset,
CORB/RIRB ring setup, interrupt enables, the stream descriptor's address
registers and the BDL's contents are bring-up mechanism with no policy in them,
and the kernel performs the sequence when soundd claims the device and hands it
buffer handles. What stays in soundd is every *decision*: which codecs answered,
which function groups, the widget-graph traversal, the pin, the converter, the
amps, EAPD, the format, and the whole mixer. That is `toyos-hda/`, unchanged by
this decision, and it is why the stub is not §4.3's kernel-resident driver
wearing a different name.

#### 4.1.2 What it costs, and what remains unmeasured

**Per period: zero syscalls.** §2.4 counts the register writes and finds none on
the driver's side; the only per-period write is the ISR's `SDnSTS`
acknowledgement, on the side of the boundary that owns the interrupt already.
The stub therefore costs nothing at all in the audio path, and §4.1's "a wake
and no syscalls" survives intact — which was the condition the owner set on it.

**Per verb: one syscall, and verbs are not in the audio path.** Through
CORB/RIRB a verb is a 32-bit store into a ring soundd may map writable — a codec
verb has no bus-mastering and cannot name memory — plus **one** `CORBWP` bump.
N verbs queued before the bump are one syscall. The immediate-command path costs
four register writes per verb (`kernel/src/drivers/hda_probe.rs:648-663`) and is
H0's, kept for a controller whose CORB is not up yet.

**At bring-up, once.** Replaying `hda_probe.rs`'s own conditional verb structure
against the committed T14 fixture gives **215 verbs** for the probe's full dump
and **115** for a driver-minimal enumeration that reads only the fields
`toyos-hda::graph::Codec` carries; QEMU's two codecs give 52 and 32. Path
configuration is 14 verbs (17 if amps are written per channel). Batched behind
one `CORBWP` bump per dependency level that is tens of syscalls, once, at claim
time — against 344.53 periods per second thereafter.

**Per jack poll: at most one syscall every 2 s** (§2.5's two verbs, one bump).
**Per volume change: zero** — the master volume is a digital gain on soundd's
mix bus and touches no codec amplifier (§8 item 12, §9).

**Unmeasured, and named so it is not read as measured.** Every figure above is a
count of syscalls, not a cost. What an MMIO read from a user address space costs
against the syscall it replaces is TCG-immeasurable (§5.1) and is H3's and H5's
to answer on real silicon in one session. The 2k+7 figure is read out of the
tree; **k̄ ≈ 1.20 is derived** from `tests/audio-baseline.toml:286`'s recorded
wake median of 918.5 over a 3.0 s tone's ~1102 periods, so completions barely
batch today and the loop pays nearly the full per-wake price on every period.

#### 4.1.3 The address surfaces, re-derived

Asked because the owner's count was six and the plan excludes one of them. What
the driver programs, from §2.1 and §2.2:

| Surface | Register(s) | Who writes it under the stub |
|---|---|---|
| CORB base | `CORBLBASE`, `CORBUBASE` | kernel, from a buffer handle |
| RIRB base | `RIRBLBASE`, `RIRBUBASE` | kernel, from a buffer handle |
| BDL base | `SDnBDPL`, `SDnBDPU` | kernel, from a buffer handle |
| the addresses *inside* BDL entries | not registers — stores into the list | kernel; `toyos-hda::stream` computes the layout, the kernel resolves handle → address |
| MSI address and data | PCI **config** space | kernel; `userspace-drivers-spec.md` §4.2 makes config space unwritable from userland and soundd never touches it |
| DMA position buffer base | `DPLBASE`, `DPUBASE` | **nobody** — §2.1's not-touched list and §8 item 8 exclude it, and the read-only mapping means soundd could not reach it if it wanted to |

**Six surfaces, three address-bearing register pairs, and the sixth is one the
plan declines to use** — so the owner's count is right and its last member is a
surface the hardware exposes rather than one the driver programs. The stub's
guarantee does not depend on this table being complete, which is exactly why it
was chosen; the table is here so a reader can see what a *writable* mapping
would have had to police.

**The complement — what soundd may ask the kernel to write — is short and every
entry carries a value that is not an address:** `CORBWP` (publish queued verbs),
`SDnCTL` (stream reset, run/stop, IOC enable, stream number), `SDnFMT` (the
16-bit format word `toyos-hda::stream` encodes), `SDnCBL` (a byte count),
`SDnLVI` (an index), and `GCTL.CRST` if a re-init is ever wanted. Verb *content*
is not on the list at all because it is a store into DMA memory soundd owns.

#### 4.1.4 What the stub does not close

- **A read-only mapping is still a 2 MiB mapping.** `alloc_and_map` asserts its
  physical base is 2 MiB-aligned (`kernel/src/mm/paging.rs:525-528`) and a
  16 KiB BAR is not, so the mapping starts at the containing boundary and
  carries whatever else lands in that page. On the T14 the candidates are the
  four sibling functions — eSPI, SMBus, **the SPI flash controller** and the
  Ethernet NIC (§3) — and a register with a read side effect among them is a
  real exposure that no amount of write-blocking removes. **The fix is a
  claim-time precondition, not a relocation**: the kernel computes the 2 MiB
  page's other occupants exactly as H0's `2m-page-neighbours` scan does and
  refuses the claim by name if there are any. That fails closed, needs no free
  MMIO space, and turns `userspace-drivers-spec.md` stage 3 from a blocker into
  the remedy for a machine that says no.
- **The T14 has never answered that scan.** H0 ran, but only §6.4's codec
  findings were committed; the whole `(a)` block — scope members, RMRR, MSI
  vectors, BAR0 size/width/movability, `2m-page-neighbours`, `ECAP.SC` — is
  **not in this repository**. §6.5 records that and what to do about it.
- **A read-only mapping is not an uncacheable one, and UC is not currently
  speakable.** `CachePolicy` has two variants and neither is UC
  (`kernel/src/mm/paging.rs:41-51`); `DeferToMtrr` is PAT entry 0, which is WB
  and takes the MTRR's type. Every MMIO mapping in this kernel already relies on
  that and the xHCI and NVMe drivers work on the T14, so firmware's MTRRs do say
  UC for this range — but that is inference from working drivers and not a read
  of `mtrr::range_type` for BAR0. §4.4 item 2's third variant is **more**
  load-bearing under the stub than it was under a full handoff: `SDnLPIB` read
  out of a write-back mapping returns a stale position, and the symptom is a
  completion mask that looks like a scheduling defect.

  **And adding it is not the one-line change it looks like.** `pat::init` writes
  the architectural reset table with entry 4 alone changed to WC —
  `ENTRIES = [WB, WT, UC-, UC, WC, WT, UC-, UC]` (`kernel/src/arch/pat.rs:37`).
  A 2 MiB PDE names its entry with `PAT<<2 | PCD<<1 | PWT`, so with PCD and PWT
  clear **only indices 0 and 4 are reachable, and both are already taken**.
  Strong UC is at 3 and 7 and needs both bits; UC- is at 2 and 6 and needs PCD.
  So an `Uncacheable` variant must set PCD and PWT, which contradicts
  `paging.rs:36-38`'s stated invariant that this kernel leaves them clear
  everywhere, *and* `from_pde`'s assert (`paging.rs:64-71`) panics on exactly
  the PDE it would install — on the very mapping the stub creates, through
  `user_policy` at `shared_memory.rs:70-73`. Reprogramming a spare slot does not
  help: every spare needs one of the two bits to be named at all.

  **Take entry 3, strong UC, not entry 2.** UC- yields WC where the MTRR says
  WC, which is the stale-position failure above rather than a defence against
  it. What the change costs: the variant, `pde_bits`, a rewritten `from_pde`
  that decodes all three bits and still refuses the four indices this kernel
  does not use, and `pat.rs:29-33`'s claim about leaving "the two bits that mean
  uncacheable to older software untouched", which stops being true.
- **`shared_memory` cannot map read-only today.** `SharedRegion::map_into`
  passes `writable: true` literally (`kernel/src/shared_memory.rs:64`) and the
  region carries no such field. One field and one argument; named because it is
  the one line of existing code the stub contradicts, and because a stub whose
  BAR mapping came out writable would look exactly like a stub that worked.

#### 4.1.6 What H2/H4 built, and the one place it differs from §4.1.1

Built on 2026-08-07. §4.1.1's decision — **the kernel never accepts an address**
— stands, and is stricter in the code than on this page. One thing changed and
the owner should read it, because it reverses a line he decided:

> **soundd does not map the BAR at all, in either direction.**

§4.1.1 item 2 kept the mapping so the per-period `SDnLPIB` read would cost no
syscall. That read moved instead: **the interrupt handler reads `SDnLPIB` and
turns it into the completion mask** — which is what §2.4 already said it should
do — so the position reaches soundd inside a record it was going to read anyway.
The mapping then had one consumer left, the verb poll, and that is two registers
rather than a 16 KiB window.

What it costs and what it buys:

- **Per period the audio path is unchanged and one syscall shorter than
  virtio's.** Measured on a boot: `submitted` is 1127 periods for the same 3 s
  tone on both backends, and HDA's loop makes no `SYS_AUDIO_SUBMIT` call at all.
  The driver performs **zero register accesses per period**; the one write is
  the ISR's `SDnSTS` acknowledgement, as §2.4 counted.
- **§4.1.4's read-side residual is gone**, and with it risk 11, the claim-time
  2 MiB-neighbour refusal, and `CachePolicy::Uncacheable`. There is no user
  mapping of device registers, so there is no page whose other occupants matter
  and no memory type to get wrong. `SharedRegion`'s `writable` field goes with
  them — the PCM ring is mapped writable, which is what it already did.
  **§6's H2 row names all three as its content and none was built**; they
  existed only to serve the mapping, which is the previous agent's finding
  ("dead code without their consumer") reaching its conclusion.
- **The driver reads two registers through the kernel**, `ICS` and `IRR`, on a
  read allow-list with the same polarity as the write one. That is a syscall per
  poll of a verb, and verbs are never in the audio path: roughly four thousand
  syscalls once at claim time, against 344.53 periods per second thereafter.

**CORB/RIRB is not built either.** Verbs go over the immediate-command
registers, which is what H0 used and what both machines in reach answer. The
ring pair buys one thing — several verbs behind one `CORBWP` write — and there
is nothing in the audio path to batch, so it would be two DMA rings and their
setup in the kernel for a saving on a path that runs once. A controller with no
immediate-command interface is a refusal by name, and §6.3's own table already
carries that line.

**The allow-list, as built.** Five writes and two reads, each on it because its
value is not an address *and* indexes nothing the kernel allocated:

| Write | Width | Why it is on the list |
|---|---|---|
| `ICW` | 32 | a codec verb, which names no memory |
| `ICS` | 16 | the immediate-command busy and valid bits |
| `SDnCTL`+0 | 8 | run and interrupt enables — **`SRST` refused**, because stream reset clears `SDnBDPL` and the driver cannot write it back |
| `SDnCTL`+2 | 8 | the stream tag, which the converter is told too |
| `SDnFMT` | 16 | the format word `toyos-hda::stream` encodes |

| Read | Width |
|---|---|
| `ICS` | 16 |
| `IRR` | 32 |

`SDnCBL` and `SDnLVI` are **not** on it, and §4.1.3's complement is wrong to
list them: both index the buffer descriptor list, and an `SDnLVI` past its end
is a DMA engine reading descriptors out of memory nobody initialised. The kernel
writes both itself from the list it built. `SDnSTS` is absent for §2.4's reason,
and a 32-bit `SDnCTL` write is refused on width alone because it reaches
`SDnSTS`.

Every arm of both lists runs at bind time under the `hda-allowlist-selftest`
kernel feature and is asserted by name in `hda_tone`. Nothing else can be the
caller: the check is gated on the device claim, soundd holds that claim for the
life of the boot, and the claim is exclusive by construction.

**Two controllers with live links is a refusal naming both.** The kernel can
tell which links answer and cannot tell which one a human is wired to — that
needs the codec graph, which is the driver's. `Profile::HdaTwoLive` and
`hda_two_live_refused` are that arm; a first-match kernel goes green on every
other HDA test.

**Nothing in §4.1.5 has fired**: no per-period register write turned up, the
list is seven entries, and the kernel's bring-up still decides nothing.

#### 4.1.5 What would reopen it

Recorded so the decision is falsifiable later (`wlan-plan.md` §3.2, same
discipline). Three of these were already here for the userland/kernel question
and still bind; the rest are the stub's own.

- A second audio device, or a capture path whose lifetime differs from
  playback's.
- **A measurement showing the MMIO reads from a user address space cost more
  than the syscalls they replaced.** Unchanged, and still unmeasured.
- **A per-period register write turning up in H4** that §2.4's count missed.
  The stub then costs one syscall per period at 344.53/s, and gate A is the
  instrument that prices it — same session, A/B, thorough tier.
- **The allow-list growing to the point where it is no longer readable as a
  list of non-addresses.** The polarity is the guarantee; a list nobody can
  check entry by entry has stopped being one.
- **A claim-time neighbour refusal on the T14** (§4.1.4). That does not reopen
  the stub — it makes `userspace-drivers-spec.md` stage 3 a prerequisite again,
  for this machine only, and §4.2 says so.
- **The kernel's bring-up sequence acquiring a decision.** Today it writes no
  policy down. The day it needs to know which codec or which pin, the line has
  moved and this section is wrong.

### 4.2 The prerequisite chain, re-derived under the stub

Not this track's work, and named so the dependency is visible. Shared with
`specs/plans/wlan-plan.md` W5 — neither track pays for it alone. **Four rows shorter
than the first draft**, and the deletions are the point of §4.1's decision.

| Prerequisite | State | What HDA needs from it, under the stub |
|---|---|---|
| `iommu-plan.md` **I0–I2** | **done** | every function has a context entry and translation is on. HDA is in the identity-mapped domain like every other kernel device, and stays there. |
| `iommu-plan.md` **I3** — interrupt remapping | not built | **not a prerequisite.** The MSI is delivered to the kernel's own IDT vector, which is where it goes today; there is no userland driver for a vector to reach. |
| `iommu-plan.md` **I4** — domains, map/unmap/invalidate | not built | **not a prerequisite.** The CORB, RIRB, BDL and audio buffers are kernel-allocated and the kernel programs their addresses. There is no untrusted IOVA to translate, because there is no address soundd can put on the bus. |
| `userspace-drivers-spec.md` **stage 3** — BAR sizing and 2 MiB re-assignment | not built | **not a prerequisite; a remedy.** A read-only mapping of a shared 2 MiB page cannot be used to write a neighbour, so relocation stops being a safety condition. It becomes what a machine failing §4.1.4's claim-time neighbour check needs in order to proceed — and the T14 has not been asked. |
| `userspace-drivers-spec.md` **stage 4** — the capability | not built | **needed, and smaller.** Claim the function, map its BAR read-only and uncacheable, hand back a PCM buffer handle, deliver IRQ records, and take allow-listed register writes. Four of §4.4's eight syscalls, not eight. |

**The IOMMU is not deleted from this picture and must not be read as optional.**
It is defence in depth against a controller that misbehaves or a kernel bug in
the stub, which is what I0–I2 already give every device. What changed is that it
is no longer the *trust boundary*: under a fully-mapped handoff the unit was the
only thing standing between soundd and physical memory, and under the stub
nothing soundd can do reaches the bus at all. That is why I3 and I4 stop gating
audio, and it is the whole reason §1's cost sentence is short now.

**Two risks close with them.** §7 risk 1 — the isolation scope's five members —
refused the handoff *as the rule is written*, and the stub does not perform a
handoff, so `iommu-spec.md` §7.3 has nothing to say about this device. §7 risk 4
— an RMRR on `00:1f.3` — refused the handoff for a hard reason, and it too has
no handoff to refuse. Both remain open questions about the machine and neither
is on audio's path any more. **The owner's research is what settled that this
was the right shape rather than a way around a rule**: refusing a device to
userspace the way Linux and VFIO do is the minority position — Fuchsia deleted
its VT-d driver in March 2026, seL4 states that DMA-capable drivers must be
trusted, and macOS merges the T14's exact PCI shape into one group — and the
stub is proven twice in the literature, by Nexus RVM/DSS (OSDI'08) against an
i810 audio controller and by Windows WDDM's `DxgkDdiPatch` and WaveRT.

**HDA is still a better vehicle for stage 4's gates than the second
`virtio-net-pci`** that spec proposes, on three independent counts, and the
spec's own §7.2 is the reason for two of them:

1. **It is behind the vIOMMU with no guest-side negotiation.** §7.2's first
   vacuity trap is that QEMU hands a virtio device the bypassing address space
   unless `iommu_platform=on`, which additionally requires the guest to
   negotiate `VIRTIO_F_ACCESS_PLATFORM` — a flag `iommu-spec.md` §11 records
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
note behind I3, whose failure mode `iommu-spec.md` §11 describes as "nothing
works, which is the hardest kind to bisect".

CLAUDE.md says effort is never an argument and that an agent must not offer a
smaller deliverable because the owner is blocked. So this plan builds the right
thing and states the cost. **The trade is the owner's and it is a sequencing
trade, not a design one** — the end state is identical either way.

**The stub is not this, and the difference is what each half decides.** The
kernel-resident driver above owns the codec enumeration, the widget-graph
traversal, the path choice, the format and the amplifiers — every decision in
§2.3, the part §5.1 calls the least-covered and highest-risk code in the plan —
and it is written to be deleted. The stub owns a fixed bring-up sequence and a
list of offsets, decides nothing, and is not written to be deleted: it is where
this device's register writes live permanently. §8 item 2 forbids the first and
is unchanged by the second. The test to apply if that distinction ever blurs is
§4.1.5's last bullet: **the day the kernel half needs to know which codec or
which pin, it has become the thing this section declines.**

### 4.4 The ABI, item by item

`userspace-drivers-spec.md` §4.6 proposes eight syscalls. **Under the stub HDA
needs four of them and one that spec does not have**, and it needs no change to
the shape of any of the four. What it adds is six things, four of which add no
ABI at all.

| # | What | General or HDA-specific | Verdict |
|---|---|---|---|
| 1 | `SYS_PCI_LIST`, `SYS_DEVICE_CLAIM`, `SYS_DEVICE_BAR_MAP`, `SYS_DEVICE_IRQ` | **general** | unchanged; HDA is their first consumer. `SYS_DEVICE_CONFIG` is not needed — soundd never reads or writes config space under the stub — and `SYS_DMA_MAP`/`SYS_DMA_UNMAP` are not either, because a driver that cannot program an address has nothing to map. `SYS_DEVICE_BAR` folds into the claim. |
| 1b | **`SYS_DEVICE_REG_WRITE(handle, offset, width, value)`** — the stub's own | **general in shape, per-device in policy** | The kernel resolves `handle` to a claimed function, checks `offset` against that device's **allow-list** and refuses anything else **by name** (§4.1.1). It is general because every stubbed device wants it; the list is the driver's. It is not `SYS_HDA_VERB`, and the rejected list below says why the distinction is real: this call names an offset and a value and knows nothing about codecs. **`width` is not a convenience** — HDA registers are 8, 16 and 32 bits and a 32-bit write to a 16-bit register is a write to its neighbour. |
| 2 | **A BAR mapping must be uncacheable, and it must be read-only** | **general** | `paging::CachePolicy` (`mm/paging.rs:41-51`) has two variants today, `DeferToMtrr` and `WriteCombining`. Neither is right for a user mapping of device registers: `DeferToMtrr` is PAT entry 0, which is WB and inherits whatever firmware's MTRRs decided (`iommu-spec.md` §11, task #139). **Adds no ABI**: a third `Uncacheable` variant, used unconditionally, because there is no BAR a driver should map write-back. A parameter here would be a signature promising a choice that has one correct answer — and the same argument applies to writability: **there is no BAR a driver should map writable under the stub**, so that is not a parameter either. What *does* need adding is the ability to express it at all: `SharedRegion::map_into` passes `writable: true` literally (`shared_memory.rs:64`) and the region carries no such field, while `AddressSpace::map_range` beneath it already honours the flag (`paging.rs:333-336`). One field, one argument, no ABI. |
| 3 | **The IRQ handle's records carry the interrupt-time timestamp and a count** | **general** | soundd's DLL exists to separate the device's period grid from scheduling latency, and it does that by timestamping *in the ISR* (`AudioCompletionRecord.timestamp_nanos`). A userland driver that timestamps at wake time folds the two back together and the DLL becomes a jitter amplifier. So a read on the IRQ handle returns `IrqRecord { count: u32, _pad: u32, timestamp_nanos: u64 }` — `AudioCompletionRecord` with `mask` replaced by `count`. **That is the completion record splitting along the trust boundary: the timestamp is the kernel's and the mask is the device's.** The mechanism to promote is `kernel/src/audio.rs`'s SPSC `RecordRing` made device-independent, **not `irq_ring`**: `irq_ring::isr_publish` coalesces a second IRQ into the existing record and **keeps the earlier timestamp** (`irq_ring.rs:57-70`), which is the right answer to "when was work first available" and the wrong one for a DLL, whose `update` measures against the *last* grid point of a batch (`soundd/src/main.rs:376`). So the 158 lines of `audio.rs` are not all deleted — its ring survives as the general mechanism. |
| 4 | **DMA must be snooped** | **the stub answers it, and the general answer becomes optional** | Intel HDA controllers carry a vendor-specific no-snoop control in PCI config space; a controller left as firmware set it may do DMA the CPU cache does not see. **The premise is read from in-tree driver behaviour and is not verified here against a datasheet — it is the risk, not the answer.** The first draft refused a config-space escape hatch and reached for VT-d's `ECAP.SC` snoop-force bit instead. **Under the stub the question dissolves**: the kernel owns config space outright, so if the bit needs clearing the kernel clears it during bring-up, beside the bus-master enable it already has to write. No escape hatch is needed because there is no wall. `ECAP.SC` stays worth having as defence in depth and stops gating this device; the T14's value is unread (§6.5). |
| 5 | **`SYS_SET_RT_PRIORITY`'s gate moves** | **general** | It is gated at the dispatch site on the `DEVICE_AUDIO` claim, which is deleted here. It becomes a right on the device-claim handle, per `capability-handles-spec.md` §6.7 — the process init gave the audio device to may enter the RT band. **No new syscall; a changed check**, and one the code already says is too weak: "That is not yet spec §9.4's privilege gate: `SYS_OPEN_DEVICE` is first-come and ungated, so whoever wins the claim race gets the RT band with it" (`scheduler.rs:308-311`). |
| 6 | **Master volume and mute** | **not kernel ABI at all** | Three messages on soundd's existing control connection: `MSG_SET_MASTER_VOLUME { gain: f32 }`, `MSG_SET_MASTER_MUTE { on: bool }`, `MSG_GET_MASTER` → `MSG_MASTER_STATE { gain, muted }`. The tempting wrong answer is a syscall; volume is a mixer's business and the mixer is a userland process. |

**Deleted by H3, as it landed**: `SYS_AUDIO_SUBMIT` (**71**), `SYS_AUDIO_POLL`
(**84**, already dead — §10 item 7), `DEVICE_AUDIO` (**4**) and its owner lock,
`Descriptor::Audio`, `AudioInfo`, and `kernel/src/audio.rs`. **All three numbers
are retired, never reused** (CLAUDE.md). `AudioCompletionRecord`, IDT vector
`0x23` and `virtio_sound.rs` itself survive, and §6.7 is why each. That deletion
closes `userspace-drivers-spec.md` stage 7.

**Rejected, named so the judgement is checkable:**

- `SYS_HDA_VERB` — a verb is a store to a ring the driver owns, published by one
  allow-listed `CORBWP` write. A syscall for the *verb* would put a device
  protocol back in the syscall table, which is the defect
  `userspace-drivers-spec.md` §7.5 check 3 exists to catch. Item 1b is not that
  and the test is exact: `SYS_DEVICE_REG_WRITE` can be implemented by a kernel
  that has never heard of a codec.
- **A refusal list of dangerous registers, instead of an allow-list.** §4.1.1.
  Same call, opposite polarity, and the failure mode of a missing entry is a
  device pointed at kernel memory rather than a stream that will not start.
- **Letting soundd write the address registers "because the IOMMU will catch
  it".** That is the fully-mapped design under another name, and it reinstates
  every row §4.2 just deleted.
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
- **Whether the T14's BAR0 shares its 2 MiB page with a sibling function's
  registers** (§4.1.4). QEMU's q35 lays out a bus this machine does not have, so
  a green neighbour check in the harness is evidence about the *check* and none
  at all about the machine. §6.5.
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
`specs/assessments/metal-track-history.md`'s lesson is that mutating your implementation
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
2. ~~**The instrument's physical scale becomes per-config.**~~ **Withdrawn by
   §6.4 item 5.** `PERIOD_SECS = 128.0/44100.0` (`tests/common/audio.rs:63`) and
   `PIPELINE_DEPTH_US = 8 × PERIOD_SECS` (`:293`) stay global: QEMU's codec
   offers 16 k–96 k at 16-bit and the T14's converter offers 44.1 k and 48 k at
   16/20/24, so both arms run 44.1 kHz S16 and share a scale. The concern was
   the right shape — it is one of the four instrument defects
   `specs/assessments/audio-gate-history.md` records — and the machine answered it.
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
  (`specs/assessments/metal-log-capture.md`). A report of "no sound" is then a diff against
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
| **H0** | **Feasibility, on metal. No driver.** A kernel diagnostic behind a boot parameter, armed only in the diagnostic image, deleted at H9. It carries the comment `specs/device-test-strategy.md` requires of an actuator, and the reason nothing else can reach it is exact: **there is no way for a userland process to touch a codec before the capability of §4 exists, and the questions it answers are the ones that decide whether that capability will ever be given this device.** Two halves on one boot. **(a) Handoff**, for `00:1f.3`: its DMAR device scope, whether its isolation scope is a singleton given four sibling functions (§7 risk 1), whether it carries an RMRR, whether it offers MSI or MSI-X and how many vectors, BAR0's size/width/prefetchability and whether a 2 MiB relocation target exists, and the unit's `ECAP.SC` (§4.4 item 4). **(b) Codec**, using the immediate-command registers so it needs no DMA and no capability: release `GCTL.CRST`, read `GCAP`/`VMIN`/`VMAJ` and `STATESTS`, and for every codec present dump vendor and device id, every function group, every widget's capabilities and every pin's configuration default. Plus: log every i8042 scancode sequence that decodes to no key, and press the three volume keys. | ~120 + ~250 lines kernel | The log carries a named line per item, read off the panel and off `/log/kernel.log`. **If the isolation scope or an RMRR refuses the device, or `STATESTS` reads zero, this track stops here and is re-decided.** |
| **H1** | **DONE — `toyos-hda`, the host-tested core.** Verb encode/decode, the graph model, `find_output_path`, `SDnFMT` encoding, BDL construction. No I/O. | ~1,500 lines Rust | `cargo test` in-crate, against three fixtures including **H0's dump of the T14's own codec**. §5.2's state-space attacks, each with teeth: deleting the cycle check must red the cyclic fixture, and deleting the speaker-pin preference must red the two-codec one. |
| **H2** | **The stub's capability.** Rewritten under §4.1's decision: I3 and I4 are **not** in it and `userspace-drivers-spec.md` stage 3 is not either (§4.2). What lands is the four syscalls of §4.4 item 1 plus `SYS_DEVICE_REG_WRITE` (item 1b), `CachePolicy::Uncacheable`, a `writable` field on `SharedRegion`, and the claim-time 2 MiB-neighbour refusal of §4.1.4. **Proposes that the capability be staged against a second `intel-hda` rather than a second `virtio-net-pci`** (§4.2). | ~600 lines kernel (est.) | The refusals, each by name: a write off the allow-list, a write of an address register, a claim whose BAR shares its page. Teeth: deleting the allow-list check must red the first two. |
| **H3** | **DONE, and §6.7 is what it actually built.** soundd drives virtio-sound from userland through the same stub HDA got: descriptor tables kernel-only, avail rings and every buffer in the region the driver maps. Deleted: `audio.rs`, `SYS_AUDIO_SUBMIT`, `SYS_AUDIO_POLL`, `DEVICE_AUDIO`, `AudioInfo`. **Not** deleted, and §6.7 says why each: `virtio_sound.rs` (it is the stub now), `AudioCompletionRecord`, vector `0x23`. Closes `userspace-drivers-spec.md` stage 7. | ~1,000 lines Rust moved, 285 lines of kernel deleted | **Gate A's thorough tier, `cargo test --test toyos-build -- --audio-gate 30`, same-session A/B against the pre-stage tree.** Same rule as a scheduler-migration transition and for the same reason. ~~This is the stage that can revert the direction, and it is deliberately before HDA exists.~~ **Spent** — H4 landed first (§6.7). |
| **H4** | **HDA behind the same seam, in QEMU.** Split by §4.1's line: **kernel stub** — reset, CORB/RIRB ring setup, interrupt enables, stream descriptor address registers, the BDL's contents, the `SDnSTS` ISR, and the allow-list. **soundd** — enumeration, path selection from H1, `SDnFMT`, the amps and EAPD, `SDnLPIB`-derived masks, zero-on-complete, the mixer. New profiles: one HDA machine, one with two controllers, one whose codec has no speaker pin, one with a controller and no codec. | ~500 kernel + ~1,500 userland (est.) | New gate-A arms (§5.3) with their own four baseline sections; `cargo test -- hda` for the shape configs. Teeth: removing the zero-on-complete write must red the phase-continuity check. **§2.3's output-preference rule stands unrelaxed** — §6.4 item 7 widened it to speaker → headphone → line-out as a policy constant before H4, and the refusal-by-name arm is what QEMU's `hda-micro` still exercises. |
| **H5** | **The T14: first note.** Flash, boot, listen. The enumeration trace lands in `/log/kernel.log`; the codec dump becomes an H1 fixture. | — | **L4, metal only.** The owner hears the 440 Hz tone from the speakers. The run's soundd counters are recorded as the T14's own first baseline. |
| **H6** | **Jack detection and output routing.** Polled pin sense on soundd's existing 2 s cadence; route between the speaker and headphone pins; ramp master gain to zero across the switch so it does not click. | ~300 lines Rust | Metal: headphones in, sound follows within one poll interval, and back out again. Harness if QEMU's codec models pin sense; if it does not, the switch logic is host-tested in `toyos-hda` and the *transition* is asserted in QEMU by driving it from a test hook. |
| **H7** | **Master volume and mute.** A gain on soundd's mix bus, §4.4 item 6's three messages, the existing ramp machinery, and `toybox audio` to read and set it. | ~400 lines Rust | **Harness, with real teeth**: a guest test sets master to 0.5 and the wav capture's amplitude halves; sets mute and the capture goes silent; neither transition fires the click detector. |
| **H8** | **The volume keys.** Three `SET1_E0` entries in `toyos-ps2` — Keyboard/Keypad-page Mute, Volume Up and Volume Down usages, against what H0 observed the EC actually sends — and the surface owners consuming those three usages instead of forwarding them (§6.1). | ~150 lines Rust | Harness: QMP injects the scancodes and a guest test asserts soundd's master state moved and the capture followed. Metal: the owner presses F1/F2/F3. |
| **H9** | **Persistence, and H0's probe deleted.** Master volume and mute survive a reboot. **Blocked** — §6.2. | ~200 lines Rust | Reboot in the harness and read the value back; on metal, only once there is a volume to write to. |
| **H10** | **The end condition.** `userspace-drivers-spec.md` §7.5's checks pass for audio; CLAUDE.md's architecture and `metal-hardware-inventory.md`'s undriven list updated; `specs/issues/` entries closed. | — | Those commands |

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

The unblocking work is `specs/plans/introspection-plan.md` W7 — the format capability
and adopt-by-witness — which is gated on the owner's review of its §4. H9 waits
on that, and until then master volume resets to its default on every boot on the
T14. In the harness it persists, because the harness has a disk.

### 6.3 H0 is built. How to run it, and how to read what comes back

**Read the (b) block first, and read it before estimating anything in §6.** The
owner has decided the T14 gets real sound out of its internal speakers. That
makes question (b) not one of four but *the* question: it decides whether this
plan is the answer to that decision or whether the answer is a project of a
different order, and no schedule for H1–H10 means anything until one real boot
has answered it.

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
cargo run -- --diag-boot --kernel-param hda-probe --build-only
```

→ `target/bootable-diag.img`. **`--kernel-param` is orthogonal to the boot
mode on purpose.** Attaching a feature list to `Boot::Diag` was the other way to
reach this image, and it would have made the diagnostic kernel permanently a
different build from the shipping one — which is the guarantee that mode exists
to make — and left a line in `src/build.rs` to take out again at H9. Here the
probe is a word on one command line and there is nothing to clean up. An
undeclared feature name is refused by name against `kernel/Cargo.toml` before
any lock, so when H9 deletes `hda-probe` this command stops working loudly
rather than quietly producing an image with no probe in it, which would look
identical and answer nothing.

**Build it from a committed tree** (CLAUDE.md): `cargo` builds the working tree,
and a checkout usually holds somebody's uncommitted work.

Flash, boot, and — **before pressing any other key** — press Mute, then Volume
Down, then Volume Up. Then mount the stick's `TOYOS-LOG` partition on the Mac
and read **this boot's own log file**: `/log` now holds one file per boot named
for the wall clock (`2033-03-07-091426.log`), so take the newest, and a machine
whose RTC would not answer writes `unknown-NN.log` instead. Everything the probe
says is a line beginning `hda:`, and the four verdict lines are `hda: (a)`,
`(b)`, `(c)`, `(d)`.

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

#### (b) Is a codec on the link — the question the whole track turns on

**There is no second way to reach those speakers.** The T14's USB bus carries
exactly four internal devices — the fingerprint reader `06cb:00bd`, the camera
`13d3:5406`, the smartcard reader `058f:9540` and the owner's boot stick
(`specs/reference/metal-hardware-inventory.md:223`, `:484`) — and **none of them is an
audio device**. `00:1f.3` is the only audio hardware on the machine. The
headphone jack is not a second path either: on this design the jack is a *pin*
on whatever answers, so it is behind exactly the same silicon as the speakers.
So "no USB DAC, no jack workaround, no third option" is not rhetoric; it is
what the inventory says.

That is why the line below decides the track rather than informing it.

**`statests` non-zero — a codec answers.**

This plan is the answer, and it is the cheap one. Everything needed is in this
file: §6's H1 through H10, whose sizes are **estimates and are labelled as such
throughout** — roughly 4,550 lines of Rust across the stages. **The prerequisite
chain this paragraph used to add to that is four rows shorter under §4.1's
stub** (§4.2): what is left is the capability itself, and `wlan-plan.md` W5 no
longer shares any of it, so audio's number is now audio's alone and smaller than
when they were pooled. No vendor firmware, no blob, no second bus. Self-hosting
(CLAUDE.md's north star) is untouched, because every line of it is ours.

**`statests=0x0000` — nothing on the legacy link. Do not soften this.**

The audio track stops being this plan and becomes a project of a different
order, and the owner has to be told that before anyone offers a date.

What it means concretely. The analogue path is behind the vendor DSP (Intel
Smart Sound), or on SoundWire, or both — §8 item 4 puts all of it out of scope
today and §2.5's last row named the risk before H0 was written. Reaching the
speakers then requires, at minimum: loading **signed vendor firmware** onto the
DSP; an IPC protocol with that firmware; a **topology** description telling it
what the machine's audio graph is; and, if the codec is on SoundWire, a link
controller and an enumeration protocol nothing in this repository has any part
of. `specs/plans/wlan-plan.md` is the shape that effort takes here — a vendor's
declarative headers as a tracked C fork, the imperative half transliterated to
Rust — and it is the right comparison because it is the only one this project
has actually costed. **No line count for SOF appears in this file and none
should be invented**: nobody here has counted it, and a plausible number in a
spec outlives every review.

Three consequences worth stating separately, because they are not the same
statement:

1. **Nothing in §6 survives.** H1's graph traversal, H4's stream engine and H0's
   own dump are all about a codec that is not there. What survives is soundd's
   mixer, gate A, and §4.2's prerequisite chain — none of which is HDA work.
2. **It collides with self-hosting in a way this plan does not.** A signed
   firmware blob is a binary ToyOS cannot build from source and never will, so
   the machine's audio would depend on an artefact outside the tree. That is a
   policy question for the owner and not an engineering detail.
3. **The doom milestone loses its last piece.** Audio on the T14 stops being a
   finishing task and becomes a track of its own, sequenced against WLAN and
   the I219 rather than after them.

**`statests` non-zero but (c) reports no speaker pin — the case that looks like
success.** A machine can carry an HDMI/DisplayPort audio codec on the legacy
link while its analogue path lives on SoundWire, and that reads as a healthy
`STATESTS`, a valid widget graph and a working output path that no human can
hear. `(c) pins reporting a speaker default device: 0` is the tell, and (d)'s
codec list is what identifies which codec answered. Treat this as the
`statests=0x0000` outcome for planning purposes: the speakers are not on this
link.

**Two readings that are not verdicts about codecs at all**, and must not be
reported as one:

| Line | What it means |
|---|---|
| `hda: (b) GCAP reads all ones` | The register window answers nothing: the function is powered down past what the probe's D0 transition reached, or hidden by firmware. Go back to firmware setup before concluding anything about codecs. |
| `hda: codecN the controller did not answer an immediate command` | `STATESTS` named a codec and the controller has no working immediate-command interface. The dump needs CORB/RIRB on this part; the codec is there and nothing above the verb layer is invalidated. |

**This generalises past this laptop.** `STATESTS` reading zero on a controller
that resets cleanly is the signature of the whole 2019-onward Intel line, where
the analogue path moved behind the DSP and the legacy link was left decoding
nothing. Any future ToyOS machine of that vintage answers the same way, so the
codec-presence read is a *platform* question and belongs in whatever eventually
replaces this probe — not filed as a T14 quirk. It is also why the industry's
direction of travel is SOF and SoundWire, and why a `statests=0x0000` on this
machine should be read as "we are early to a problem everyone else has already
had" rather than as bad luck.

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

### 6.4 What H0's boot changed in H1, and the one thing it changed in H4

H0 ran on the T14 on 2026-08-05. `STATESTS=0x0005`: an ALC257 at codec 0 and
Intel display audio at codec 2. **§7 risk 2 is closed on this machine and
§6.3's SOF/SoundWire branch does not fire.** The graph:

```
DAC 0x02 ──┬──> pin 0x14  fixed, speaker,  assoc 1 seq 0    ← internal speaker
           └──> pin 0x21  jack,  hp-out,   assoc 1 seq 15   ← headphone
DAC 0x03 ──────┘ (index 1 of 0x21 only)
```

`toyos-hda/` is built against it. Seven things the machine settled that this
file had only argued:

1. **EAPD is missing from §2.3 step 6 and is not optional.** Both output pins
   report EAPD-capable and read the bit back clear at boot, so a path
   configured exactly as step 6 describes makes no sound. `PinSetup::eapd`
   carries the obligation.
2. **Mute lives at the pin and gain at the converter, and they do not swap.**
   The converter's amplifier has 88 steps of 0.75 dB and **bit 31 clear** — no
   mute at all — while both pin amplifiers are mute-only with one step.
   `AmpCaps::gain` is an `Option`, so there is no 0 dB index to write where
   none exists.
3. **The trap on this machine is pin 0x1b, not the display codec.** Four pins
   call themselves speakers with no physical connection; 0x1b has a valid
   connection list, an output amplifier, and traces to a converter perfectly.
   Only port connectivity separates it from the real speaker. The display codec
   sits at the *higher* address, so a first-match driver gets the right codec
   here by luck — the rule stands, and this machine does not enforce it.
4. **The traversal has no metal coverage.** Both output paths are depth 1, so
   the cycle check, the depth bound and selector handling are exercised by
   synthetic graphs and by nothing else. §5.2's fixtures are not a supplement;
   they are the only coverage the general algorithm has.
5. **Risk 8 is closed on both arms.** The T14's converter offers 44.1 kHz and
   48 kHz at 16/20/24-bit; QEMU's offers 16 k–96 k at 16-bit. Both do 44.1 kHz
   S16, so **§5.3 item 2's per-config physical scale is not needed** and gate
   A's existing constants serve both backends.

   **And asking for it is not the same as getting it.** `stream_format` put the
   sample-base bit at 13, where the field's multiplier is, so H4 asked both
   machines for 44.1 kHz with `0x2011` — a 48 kHz base carrying a reserved
   multiplier — and both played 48 kHz. Every buffer was correct and the whole
   pipeline ran 8.8% fast. Closed 2026-08-08; the evidence, the two gates and
   what it had been hiding in the `hda_tone` capture are in
   `specs/issues/audio/` and `specs/assessments/metal-logs/2026-08-08-audio-underruns/`.
6. **Association is the codec's own statement** that the speaker and the jack
   are one output: both association 1, the jack last by sequence.
7. **QEMU has no speaker pin, so §2.3's rule is widened. Decided 2026-08-06.**
   Both `hda-output` and `hda-duplex` fix their configuration default at
   line-out and no device property changes it, so a speaker-only rule refuses
   the harness's own machine.

   **The driver prefers speaker, then headphone-out, then line-out.** What
   that states is *an output that reaches a human*, and a machine with no
   speaker pin is a real configuration rather than a device to refuse — QEMU's
   is one, a box with nothing but a jack on the back is another. It changes
   nothing on the T14, where the speaker is present and comes first.

   Two conditions on it, both met by `toyos-hda`:

   - The order is **one named policy constant**, `path::OUTPUT_PREFERENCE`,
     rather than a fallback chain through the traversal. Reversing it reds
     three tests.
   - **The refusal-by-name arm stays**, as the test of the *no output at all*
     case. Display audio's pin is `DigitalOtherOut`, deliberately absent from
     the preference, so a codec offering only that is still refused with every
     codec address named.

   What it changed in the crate beyond the constant: `OutputPath.speaker`
   became `.output` with a `.device` beside it. A field named `speaker`
   holding a line-out is the lie the comment rule is about, and a driver that
   bound one has to be able to say so in its log.

### 6.6 Where the stages actually stand, 2026-08-07

**H2 and H4 are built and H3 is not**, which is a re-ordering §6's table does not
contain and which the owner should accept or reject. What H3 does is move
*virtio-sound* into userland and delete the kernel's audio path; what H4 needs
from it is a backend seam in soundd, and that seam is 90 lines and is now there
with two implementations. So HDA reached the wire without the deletion, and the
deletion is now a cleanup of a path the T14 does not have rather than a
prerequisite for the machine that does. **The risk H3 exists to price — that the
userspace boundary costs more than audio can pay (risk 3) — is not measured by
this, and is still open.**

What is built:

- **H2**, as §4.1.6 rewrote it: two syscalls (`SYS_DEVICE_REG_READ` 97,
  `SYS_DEVICE_REG_WRITE` 98), a `DeviceType::HdaAudio` claim carrying `HdaInfo`,
  and the allow-list. Not built, and §4.1.6 says why each is now unnecessary:
  `CachePolicy::Uncacheable`, `SharedRegion`'s `writable` field, the claim-time
  2 MiB-neighbour refusal.
- **H4**, both halves. `toyos-hda` grew `probe::enumerate` (the walk, driven
  through one `Verbs` method, host-tested against the committed H0 logs of both
  machines) and `config::verbs` (§2.3 step 6 as a pure function). 81 host tests,
  up from 64.
- **The instrument §5.3 item 5 asked for**: `audio::phase_breaks`, which reads 0
  on all four recorded virtio configs and 8–16 on the HDA arm. That is
  `specs/issues/audio/`'s declared red and the largest open thing in this track.

What is **not** built, and is H4's own gate as §5.3 states it: the four new
`tests/audio-baseline.toml` sections. Recording them is 30 invocations of four
configs on a quiet session, and this branch has no such sample — so `hda_tone`
asserts *harm* (a tone at full amplitude, no mid-tone silence, and soundd's
counters) and claims no distribution. A thorough tier for the HDA arm needs
those sections first.

### 6.7 What H3 built, and the two things §6's row got wrong

Built on 2026-08-07, after H2 and H4. Two of the row's claims do not survive
§4.1.1's decision and one of its properties was already spent; the rest holds.

> **virtio-sound did not leave the kernel, because under the stub no driver
> does.** `kernel/src/drivers/virtio_sound.rs` is not deleted and cannot be: the
> stub is the shape where a device keeps a kernel bring-up half permanently
> (§4.3's last paragraph). It went from 638 lines to 512, and what left it is
> every *decision*.

**The line, on this device.** A split virtqueue names memory in exactly one
place — its descriptor table — so that is what stays kernel-only, and virtio
1.0's three separate address registers are what make the split expressible at all
(`VirtqueueRegions::from_separate`, which `virtio_net`'s RX queue already used).
The kernel allocates two regions: a page nobody maps, holding the three
descriptor tables and the TX used ring; and the region the driver maps writable,
holding the PCM periods, the three avail rings, the control and event used rings,
and every request, response, transfer header, status and event buffer. Every
chain is built once at bind out of offsets into the second region
(`build_chains`), so after bring-up **there is no descriptor left to write** and
the driver's whole vocabulary is an index into an avail ring and a doorbell.

The TX used ring is the one the driver never sees, and that is not tidiness: the
handler derives the completion mask from it and timestamps it, which is §4.4 item
3's requirement, and a mask derived from a ring userland could rewrite is a
completion for a period that never played. A used entry naming a descriptor that
heads no chain is therefore untrusted input — counted, and named once from the
drain path, never asserted on.

**The allow-list is three entries and they are the three doorbells.** Each
carries the same property HDA's entries do: its value is a queue index, which
names no memory, and *which* queue is already decided by which offset was named.
The read list is **empty** — this driver reads no register at all, because the
device's answers reach it through memory it maps.

**What a period costs is unchanged.** `SYS_AUDIO_SUBMIT` is gone and one
`SYS_DEVICE_REG_WRITE` of a doorbell replaces it: the same one syscall, moved
from a call naming a buffer index to one naming a register offset. The avail-ring
store that used to happen inside it is now a store in soundd's own address space.
§4.1.2's 2k+7 per wake is 2k+7 still, so H3 measures the boundary rather than
paying for it.

**Deleted:** `kernel/src/audio.rs` (159 lines), `SYS_AUDIO_SUBMIT` (**71**),
`SYS_AUDIO_POLL` (**84** — dead ABI already, so this saves nothing and is
recorded because the number is now spoken for), `DeviceType::Audio` (**4**,
retired rather than reused for the stub that replaced it: the new claim
authorizes register writes and answers no submit, so a caller naming 4 has to be
refused rather than handed a capability of a different shape), `AudioInfo`,
`Descriptor::Audio` and the byte written to its fd to start and stop a stream,
`AudioDev`, `DescSlot::reclaim` and `Virtqueue::initial_slots_strided`.

**Not deleted, against the row:**

- **`AudioCompletionRecord`.** Both stubs produce it, and H4 already had HDA
  producing it. It is *the* record, not virtio's.
- **IDT vector `0x23`.** The stub still owns the interrupt — that is the half of
  the boundary that owns the IDT. Retiring it would mean a new number for the
  same device's MSI-X.
- **`kernel/src/drivers/virtio_sound.rs`.** Above.

**`RegWidth` moved from `toyos_abi::hda` to `toyos_abi::syscall`**, beside the
two calls whose argument it is. Two device families answer them now, which is the
first evidence for §4.4 item 1b's claim that `SYS_DEVICE_REG_WRITE` is general in
shape rather than HDA's syscall wearing a general name.

**The property H3 was designed to have is spent, and this file should not imply
otherwise.** §6's row calls it "the stage that can revert the direction, and it
is deliberately before HDA exists". HDA exists and is landed (§6.6), so a red
here can no longer un-decide the userspace-driver direction — it can only say
this move made guest audio worse. Risk 3 is still what the gate prices; what is
gone is the cheap exit.

**And the gate did not price it, because the instrument was down.** The A arm —
`main`, no delta — failed the thorough tier on its own at 10 dropouts in 120 runs
against a recorded 0/120, which is `specs/issues/audio/`'s new entry and the rate
that section had been asking for. Both B-arm attempts then stopped at iterations
2 and 4 on a kernel double-panic in the TLB shootdown work that landed between
the arms (§3). So **risk 3 remains unmeasured**, and the accounting above — one
syscall per period, before and after — is what H3 actually delivers on it. What
the change has instead is a full suite at 289/289 with all four audio configs
clean, and ten standalone runs of the audio family; none of that is a rate.

### 6.5 What H0's boot did *not* leave behind

**H0's `(a)` block is not in this repository.** §6.4 records seven findings and
every one of them is from the codec dump; nothing about the handoff half was
committed. So the following are unread as far as this tree is concerned, and a
reader must not take §6.3's answer table for a record of what the machine said:

| Unrecorded | Who wants it now |
|---|---|
| DMAR device scope members for `00:1f.3` | nobody on this track any more (§4.2). The owner read five members off the boot; `iommu-spec.md` §7.3 is where that belongs, and `wlan-plan.md` still wants it for the I219. |
| RMRR presence | same |
| MSI vector count, MSI-X presence | H2 — the stub still needs one vector, and `msi=none msix=none` would be the one answer that stops it |
| BAR0 size, width, prefetchability, movability | H2 |
| **`2m-page-neighbours`** | **H2, and it is the only one the stub genuinely turns on** (§4.1.4). A nonzero count is a claim-time refusal on the T14 and makes `userspace-drivers-spec.md` stage 3 a prerequisite for that machine. |
| `ECAP.SC` | defence in depth only (§4.4 item 4) |

**And one question H0 could not have answered, because the probe never asks it.**
`size_bar0` reads exactly one BAR (`kernel/src/drivers/hda_probe.rs:448-453`),
and the only loop over BAR indices skips the HDA function itself
(`:382-384`). Nothing else in the boot path prints a BAR for `00:1f.3`. So
**whether `00:1f.3` exposes a second BAR — the Audio DSP window Intel parts of
this generation carry — is unanswered here**, and no amount of re-reading the
existing log will answer it.

That matters for §8 item 4's exclusion, and the stub is what makes it survivable
rather than urgent: **soundd maps what it is given and it is given BAR0
read-only.** A second BAR it never maps is a surface it cannot reach, and a
second BAR it *were* given would still be unwritable. The exclusion therefore
holds under the stub for a stronger reason than it held before — it used to
depend on the DSP being absent, and now it depends only on the kernel not
mapping it. The enumeration is still worth having, because a driver ought to be
able to say what its device exposes: **one loop over BAR indices 0–5 of the
probed function**, on the next diagnostic boot, and the log line already has a
shape to follow.

---

## 7. Open risks

Each with what settles it, and how early.

1. **CLOSED FOR AUDIO by §4.1's stub — still open for the I219.**
   `iommu-spec.md` §7.3 hands a device to userspace only if its isolation scope
   is a singleton, and names multi-function devices as one of the two ways that
   fails. The T14's HDA is **function 3 of a five-function device** whose other
   members are the eSPI bridge, SMBus, the SPI flash controller and the Ethernet
   NIC (§3), and the owner read **five scope members** off H0's boot — a number
   this repository does not otherwise record (§6.5). §7.3 refuses that device as
   the rule is written. **The stub performs no handoff, so the rule has nothing
   to refuse**: `00:1f.3` stays a kernel device with a read-only window into
   userland. The rule still refuses `00:1f.6` — **gate N's metal target
   (`wlan-plan.md` §7)** — and restating it remains `iommu-spec.md`'s decision.
   What changed is that audio no longer waits on it.
2. **The codec may not be on the HDA link, and this is now the whole track's
   risk rather than one of ten.** §3, and §6.3's (b) block for what each answer
   costs. The owner has decided the T14 gets real internal-speaker audio, and
   the machine has no USB audio device and no jack that is not a pin on the
   same silicon — so `STATESTS` reading zero does not merely end this plan, it
   converts the audio track into an SOF/SoundWire project with vendor firmware
   in it. **Settled by H0's second line, and nothing in §6 should be estimated
   before it is read.**
3. **The userspace-driver boundary may cost more than audio can pay.**
   `userspace-drivers-spec.md` §9 already predicts stage 7 is the one most
   likely to be reverted, and gate A's thorough tier at N=30 does not detect a
   doubling of the dropout rate. §4.1 argues the path gets *shorter*, not
   longer, and that argument is unmeasured. **Priced by H3**, whose accounting
   says the virtio period costs the same one syscall it always did (§6.7), and
   whose honest failure mode is that the gate goes green while something got
   worse below its resolution. ~~Deliberately before HDA exists~~ — H4 landed
   first, so a red here names a regression and no longer reverts a direction.
4. **CLOSED FOR AUDIO — an RMRR on `00:1f.3`.** `iommu-spec.md` §7.4 refuses a
   device carrying one **for userspace handoff**, and there is no handoff. A
   kernel device's RMRRs are satisfied for free by the identity-mapped domain it
   is already in (§7.4's own first rule). Still unread (§6.5) and still the
   I219's problem.
5. **MSI presence.** The stub needs one vector like every kernel driver, so
   `msi=none msix=none` is the only answer that bites — and no real part is
   built that way (§6.3). BAR *relocatability* is now only the remedy for risk
   11. Unread (§6.5).
6. **DOWNGRADED — `ECAP.SC` may be clear on the T14's unit.** §4.4 item 4's
   answer under the stub is a config-space write the kernel performs itself, so
   snoop-force is defence in depth rather than the mechanism. Unread (§6.5).
7. **PARTLY REALISED — Gate A's instrument cannot see a repeated period.**
   §2.4's zero-on-complete rule keeps the gap detector valid across both
   backends and remains a design promise. What it does not cover is the free
   list's *meaning*, and that is where the T14 killed soundd: three mix-loop
   rules assumed a period soundd holds is one the device does not have
   (`specs/issues/audio/`). Gate A saw none of it — its clients keep their rings full
   — so the gate is `hda_client_stall`, whose actuator is a client that stops.
   §5.3 item 5's phase check is built and is separately red (#88).
8. **CLOSED — QEMU's codec may not offer 44,100 Hz.** It does, at 16-bit, and so
   does the T14's converter (§6.4 item 5). Gate A's existing constants serve
   both backends and §5.3 item 2's per-config scale is not needed.
9. **The graph traversal is the least-covered code in the plan**, and its
   fixture coverage is one real machine's codec. A second machine would answer a
   different question; there is not one.
10. **Nothing here is measurable against CLAUDE.md's 2× bar.** §5.1.
11. **NEW — BAR0 may share its 2 MiB page with a sibling function.** §4.1.4: the
    read-only mapping stops it being a *write* surface and does not stop it
    being a read one, and one of the candidates is the SPI flash controller. The
    claim-time refusal fails closed, so the failure mode is "no audio on the
    T14 until stage 3 lands" rather than "audio and an exposure". **Unread**
    (§6.5), and it is the one H0 question the stub genuinely turns on.
12. **NEW — the allow-list is a human-checked property.** Every entry is on it
    because its value is not an address, and nothing compiles that claim.
    `SDnFMT` and `SDnCBL` are neighbours of `SDnBDPL` in the same descriptor;
    a wrong offset or a wrong width in the table is a hole with no test that
    fails. §4.4 item 1b's `width` argument is half the mitigation and a per-entry
    review is the other half. The one thing that would make it unrepresentable —
    a generated table from a register description — is not proposed here,
    because there are eight entries.
13. **NEW — `Uncacheable` is not in `CachePolicy` and cannot be added without
    breaking the kernel's PCD/PWT invariant.** §4.1.4 has the arithmetic. A BAR
    mapped today would be write-back and `SDnLPIB` would read stale, which is
    the failure that looks most like a scheduler bug. Not a research question,
    but larger than one variant, and it touches an assert that fires on the
    stub's own mapping.

---

## 8. Explicitly not doing

1. **Capture.** No microphone, no ADC path, no input pins. It also removes the
   only route to a self-verifying metal boot (§5.4), and that is the cost.
2. **A kernel-resident HDA driver**, at any point, even temporarily. §4.3 prices
   it for the owner and declines it. **The stub is not it** — the test is
   whether the kernel half decides anything, and §4.3's last paragraph is where
   that is argued and §4.1.5's last bullet is what would falsify it.
3. **A separate HDA daemon.** §4.1: the driver is a library in soundd.
4. **The DSP / Smart Sound block**, vendor topologies, and firmware loading for
   any of it. **The exclusion survives the stub and is stronger under it**: it
   used to rest on the DSP being absent, and now it rests on the kernel not
   mapping the window it would live in. Whether `00:1f.3` even exposes a second
   BAR is unread and unreadable from the existing log (§6.5).
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
    `SYS_OPEN_DEVICE`'s first-come claim already has (`specs/issues/isolation/`).
    Closing it is `capability-handles-spec.md`'s work.
12. **A codec amplifier as the master volume control.** §9.
13. **Mapping any BAR writable into userland**, on this device or any other.
    §4.1.1. Named as an exclusion and not only as a design, because it is the
    one line that — relaxed once, for convenience, on some later device —
    restores every prerequisite §4.2 deleted and does so silently. The
    unrepresentable form is available and should be taken: the BAR-mapping call
    has no writability parameter (§4.4 item 2).
14. **A refusal list of address-bearing registers.** §4.4's rejected list. The
    allow-list is the same table with the opposite polarity and a bounded
    failure mode.

---

## 9. Where I think this is wrong

A plan that cannot say no is not a plan.

- **The stub puts a fixed HDA bring-up sequence in the kernel and calls it
  mechanism.** That is a judgement, not a fact. CORB/RIRB ring setup and stream
  descriptor programming are HDA-specific code in `kernel/`, and the defence —
  that it decides nothing — is a property somebody has to keep true. §4.1.5's
  last bullet is the tripwire and it has no test behind it. **The honest reading
  is that §8 item 2's line moved, and the argument for where it moved to is
  §4.3's last paragraph rather than a rule.**
- **"The kernel never accepts an address" is only as good as the allow-list.**
  Risk 12. Eight entries, human-checked, and a wrong offset is a hole nothing
  reds.
- **The read-side residual is real and is being deferred to a check nobody has
  run.** §4.1.4: a read-only mapping of a 2 MiB page containing the SPI flash
  controller's registers is not nothing, and the claim-time refusal that closes
  it has never been asked of the machine it is for (§6.5).
- **The stub was not measured against the alternative it replaced**, because
  neither exists. §4.1.2 counts syscalls and stops there; that the audio path is
  unchanged per period is an accounting result, and whether an MMIO read from a
  user address space beats the syscall it replaces is still unmeasured (risk 3).

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

1. **`specs/reference/metal-hardware-inventory.md:507-508` prices an HDA driver as
   "CORB/RIRB, stream descriptors, codec enumeration".** True, and incomplete in
   the way that matters: it omits the widget-graph traversal, which §5.1 argues
   is the least-covered and highest-risk part, and it omits that the *codec* is
   invisible to the boot log that entry was derived from — so the entry cannot
   say whether the machine has one.
2. **`specs/plans/userspace-drivers-spec.md` §6 stage 4 stages the capability against
   a second `virtio-net-pci`.** §4.2 here gives three reasons an `intel-hda`
   is a strictly better vehicle, two of which are that spec's own §7.2 vacuity
   traps not applying to a non-virtio device using plain MSI.
3. **`specs/iommu-spec.md` §8 and `specs/plans/userspace-drivers-spec.md` §7.2 record
   QEMU 11.0.2, measured 2026-08-02.** This host now reports **11.0.3**
   (`qemu-system-x86_64 --version`, 2026-08-04). No measurement in either file
   is known to have changed; the version in them is simply no longer this host's.
4. ~~**`tests/common/audio.rs`'s `PERIOD_SECS` and `PIPELINE_DEPTH_US` are global
   constants** derived from one backend's 128-frame period at 44,100 Hz.~~
   **Withdrawn by §6.4 item 5**: both codecs offer 44.1 kHz S16, so the two
   backends share a physical scale and the constants stay global. §5.3 item 2
   and risk 8 are the same withdrawal.
5. ~~**soundd's mix loop carries a comment ordering `soundd: resumed` before "the
   kernel's own `virtio-sound: stream 0 started` line".**~~ **Done at H3.** The
   line is soundd's own now; it and `stream 0 stopped` keep their exact text,
   because `check_suspend_structure` and `audio_idle_suspend` are written against
   both and the events they name did not move.
6. ~~**soundd's suspend block anticipates exactly this device**: "advertised
   per-backend through `AudioInfo`."~~ **Done at H3**: `AudioInfo` is deleted and
   the comment names the backend trait. Whether HDA pops on stop is still a
   hardware property and still H5's to listen for.
7. **`SYS_AUDIO_POLL` (84) is dead ABI and this file used to cite it as a live
   cost.** It is declared at `toyos-abi/src/syscall.rs:65` and appears nowhere
   else in the tree — no dispatch arm, no wrapper, no caller — so a call lands
   on `arch/syscall.rs`'s `_ => InvalidArgument`.
   `capability-handles-spec.md:759` already lists it as dead. soundd reads
   completion records with `SYS_READ_NONBLOCK` (66) on the audio device fd.
   §4.1 and §4.4's deletion list are corrected.
8. **§4.1's "a period costs a wake plus two syscalls" understated it by a
   factor of about four.** Counted off the mix loop: **2k + 7 per wake**, k the
   periods refilled — one `SYS_IO_URING_ENTER`, one `SYS_READ_NONBLOCK`, one
   `SYS_WRITE_NONBLOCK` per client, four `SYS_CLOCK`, plus one `SYS_CLOCK` and
   one `SYS_AUDIO_SUBMIT` per period. Three of the four fixed clock reads exist
   for statistics and pipeline bookkeeping. **k̄ ≈ 1.20**, derived from
   `tests/audio-baseline.toml:286`'s recorded wake median of 918.5 against the
   ~1102 periods of a 3.0 s tone — so ~7.8 syscalls per period today, and the
   batch factor rather than the syscall count is the lever. Nothing in §4.1's
   *conclusion* changes; the number it rested on was wrong in the direction that
   flatters the conclusion, which is the direction worth correcting.
