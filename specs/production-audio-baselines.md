# Production audio and scheduler baselines

Researched 2026-07-29. External research; no ToyOS code was built, run or changed
to produce it.

## 0. Why this file exists, and how to read it

The owner's requirement is a ratio: *"the soundsystem and scheduler needs to be on
par with linux and other oses performance wise. every component must at most be
100% (2x) slower / inefficient than current day productive oses."*

A ratio needs a denominator. This file is the denominator, with sources, so that
"we are within 2x" stops being a claim about remembered ballparks. Numbers
previously quoted from memory elsewhere in the tree should be replaced by these.

Three markings appear throughout and all three are load-bearing.

**Source class.** `[P]` = primary: vendor documentation, upstream source code, or a
peer-reviewed measurement paper that states its own hardware and method. `[S]` =
secondary: a maintainer's slide deck reported second-hand, a forum measurement, a
well-documented blog benchmark. Nothing in the summary table rests on `[S]` alone.

**Configuration dependence.** Every audio latency figure is meaningless without its
period size, and most scheduling-latency figures move by two orders of magnitude
with kernel config and load. Where a single number would be a lie, a range with its
conditions is given instead.

**Comparability verdict.** Each metric section ends with an explicit statement of
whether ToyOS's number can be honestly compared today. This is not a footnote. ToyOS
runs only under cross-arch QEMU TCG on Apple silicon — `qemu-system-x86_64 -accel
help` reports `tcg` only on this host, Hypervisor.framework virtualizes ARM64 guests
only, and `src/qemu.rs` gates KVM on `cfg!(target_arch = "x86_64")`, false on
darwin/arm64 (`specs/arm64-research-2026-07-28.md` §"Verified on this machine").
TCG's distortion is **non-uniform**, measured on this host:

| Operation | TCG (x86-64) vs native | Source |
|---|---|---|
| Address-space switch + TLB flush | 1619 ns vs 251 ns = **6.5x** | `specs/arm64-research-2026-07-28.md` |
| Memory | **3.4x** | ibid. |
| Real atomics (smp>=2) | **~1.4x** | ibid. |
| Dependency-bound ALU loop | **1.06x** | ibid. |
| Device MMIO | 168 ns TCG vs 876 ns HVF = **0.19x** (TCG is *faster*) | ibid. |

There is no scale factor that recovers a bare-metal number from a TCG one, because
the components of any real code path are inflated by factors spanning 1.06x to 6.5x
in one direction and 0.19x in the other. A metric dominated by context switching
cannot be compared to bare-metal Linux through that lens. A metric that is a
structural yes/no can.

Compounding it, and *not* a TCG effect: the kernel is built at opt-level 0 with
debug-assertions and overflow-checks, and a release kernel has never been built in
this tree (`specs/cpu-attribution.md`; `kernel/target/x86_64-unknown-none/` contains
only `debug/`). Userland meanwhile builds every dependency at opt-level 2. Any
absolute ToyOS CPU-cost number is inflated by an unknown but large factor that holds
identically on real hardware.

---

## 1. ToyOS's side of the comparison

Stated once, here, so each metric section can just reference it. All from the tree.

| Quantity | Value | Where |
|---|---|---|
| Device sample rate | 44 100 Hz | `kernel/src/drivers/virtio_sound.rs` |
| Device period | `PERIOD_BYTES = 512` = 128 frames stereo i16 = **2.902 ms** | ibid. |
| Device pipeline | `TX_INFLIGHT_MAX = 8` periods = **23.219 ms** | ibid. |
| Client ring | `slot_count = num_buffers` = 8 periods = **23.219 ms** | `userland/soundd/src/main.rs` |
| soundd DLL bandwidth | `bw = 0.03` | `userland/soundd/src/main.rs:334` |
| soundd idle cost, no clients | **~7% of a core** | CLAUDE.md known issue |
| soundd idle wake cadence | ~43/s (one per whole pipeline; `timeout = u64::MAX`) | ibid. |
| Recorded wake lateness, `audio_tone.smp1` | median 9 418 us, max 56 909 us (2.45 pipelines) | `tests/audio-baseline.toml` |
| Recorded wake lateness, other configs | medians ~7.6-9.9 ms | ibid. |
| Recorded underruns / drains / dropouts | 0 on all 120 config-runs | ibid. |
| Context-switch cost, end to end | **never measured** | - |

The recorded wake-lateness sample was taken on an Apple M4 Pro, QEMU 11.0.2,
cross-arch TCG, quiet host, 30 serial runs per config. That provenance matters for
every comparison below.

---

## 2. Metric 1 - audio output latency, server to device

A latency figure without its period size is not a figure. Each row states its
configuration.

### 2.1 PipeWire

Defaults, from `pipewire.conf(5)` CONTEXT PROPERTIES `[P]`:

```
default.clock.rate      = 48000
default.clock.quantum   = 1024
default.clock.min-quantum = 32
default.clock.max-quantum = 8192
```

and from `pipewire-props(7)` `[P]`: `node.latency = 1024/48000`, described as "a
suggested latency on the node as a fraction... the graph will try to configure this
latency or less".

- **Desktop default**: quantum 1024 / 48 000 = **21.33 ms**.
- **Pro-audio floor**: min-quantum 32 / 48 000 = **0.67 ms**. Commonly used
  pro settings are 64/48000 = 1.33 ms and 256/48000 = 5.33 ms.
- PipeWire's author's own summary slide claims "RT capable, low latency (<1.5ms)"
  (FOSDEM 2019, slide 21) `[P]`.

**Condition that is easy to miss.** The quantum is the *graph* cycle, not the wire
depth. The ALSA sink additionally holds device periods plus headroom:
`api.alsa.period-size` defaults to 0, documented as "open the device in the default
period size to minimize latency", and `api.alsa.period-num` defaults to 0, "use as
many as possible" `[P pipewire-props(7)]`. So the true speaker-side latency of a
default desktop PipeWire is >= 21.33 ms and driver-dependent. Anyone quoting 21.33 ms
as an end-to-end figure is quoting the quantum.

### 2.2 JACK

From `jackd(1)`, ALSA backend `[P]`:

- `-p/--period` default **1024**, `-n/--nperiods` default **2**, `-r/--rate` default
  **48000**.
- "The JACK output latency in seconds is --nperiods times --period divided by --rate."
- "The JACK capture latency in seconds is --period divided by --rate."

So:

| Configuration | Output latency | Capture latency |
|---|---|---|
| Default `-p1024 -n2 -r48000` | **42.67 ms** | 21.33 ms |
| Typical pro `-p128 -n2` | 5.33 ms | 2.67 ms |
| Aggressive `-p64 -n2` | **2.67 ms** | 1.33 ms |

Note JACK2 in asynchronous mode buffers `(n+1)*p` rather than `n*p` `[S]` - a
one-period penalty that shows up in measured round trips and not in the formula.

### 2.3 WASAPI (Windows)

All from Microsoft Learn, *Low Latency Audio* (windows-hardware/drivers/audio) `[P]`:

- Audio engine latency: "Before Windows 10, the latency of the audio engine was
  equal to ~12 ms for applications that use floating point data and ~6 ms for
  applications that use integer data... In Windows 10 and later, the latency has been
  reduced to **1.3 ms** for all applications."
- Buffer: "Before Windows 10, the buffer was always set to ~10 ms. Starting with
  Windows 10, the buffer size is defined by the audio driver."
- Default shared-mode behaviour is unchanged: "by default all applications in Windows
  10 and later will use **10-ms buffers**". An app must call `IAudioClient3::
  InitializeSharedAudioStream` (or AudioGraph `LowestLatency`) to get less.
- Driver floor on the inbox stack: "The inbox HDAudio driver has been updated to
  support buffer sizes between **128 samples (2.66ms@48kHz)** and **480 samples
  (10ms@48kHz)**." The sysvad sample declares "2 ms minimum processing interval" as
  the absolute driver minimum with 128 samples for the default processing mode.
- Exclusive mode: "the data bypasses the audio engine and goes directly from the
  application to the buffer where the driver reads it from" - so exclusive-mode
  latency is the driver buffer alone, the same 2.66-10 ms range on the inbox driver,
  and lower with a vendor driver.

| Configuration | Render path latency |
|---|---|
| Shared, default | ~10 ms buffer + 1.3 ms engine = **~11.3 ms** |
| Shared, `IAudioClient3` min on inbox HDAudio | 2.66 ms + 1.3 ms = **~4.0 ms** |
| Exclusive, inbox HDAudio min | **2.66 ms** |

The Microsoft page carries a round-trip measurement graph for WASAPI vs AudioGraph
across buffer sizes on a Haswell system with the inbox HDAudio driver. The graph is
an image and its numbers could not be extracted; the caveats it documents are that
AudioGraph adds one buffer on capture, plus another on render above 6 ms buffers.

### 2.4 CoreAudio (macOS)

- Default I/O buffer: **512 sample frames**. Apple TN2321 `[P]`: "With OS X 10.9,
  setting this hint will increase the default I/O buffer size from `512` sample
  frames to `4096` sample frames" - i.e. 512 is the standing default and 4096 is the
  opt-in power-saving value.
  - 512 / 44 100 = **11.61 ms**; 512 / 48 000 = **10.67 ms**.
  - 4096 / 48 000 = 85.3 ms under the power hint.
- Floor is device-driven: the legal range is queried via
  `kAudioDevicePropertyBufferFrameSizeRange` `[P AudioHardware.h]`. Pro interfaces
  advertise down to 32 frames (0.67 ms @48k) `[S]`.
- Add the device's safety offset and hardware latency, both exposed as HAL
  properties; they are per-device and not quotable as a single number.

### 2.5 The 2x line, and ToyOS against it

ToyOS's server-to-device depth is the DMA pipeline: **23.219 ms**, fixed. The
client-to-server ring adds another 23.219 ms, so client-to-speaker is up to
**46.4 ms**.

| Reference (default configs) | Figure | 2x line | ToyOS server->device | ToyOS client->speaker (worst case) |
|---|---|---|---|---|
| WASAPI shared default | 11.3 ms | 22.6 ms | 23.2 ms - **marginal fail** | 46.4 ms - fail |
| CoreAudio default (44.1k) | 11.6 ms | 23.2 ms | 23.2 ms - **exactly on the line** | 46.4 ms - fail |
| PipeWire desktop default | 21.3 ms | 42.7 ms | 23.2 ms - pass | 46.4 ms - marginal fail |
| JACK default output | 42.7 ms | 85.3 ms | 23.2 ms - pass | 46.4 ms - pass |

The client-side 23.219 ms is the depth of a *full* slot ring, so 46.4 ms is a worst
case rather than a steady-state measurement - a client that runs one slot ahead sees
much less. It is the right number to compare against production defaults anyway,
because every production figure above is also the configured buffer depth and not an
observed fill level.

| Reference (low-latency configs) | Figure | 2x line | ToyOS |
|---|---|---|---|
| PipeWire min-quantum | 0.67 ms | 1.33 ms | no such configuration |
| JACK `-p64 -n2` | 2.67 ms | 5.33 ms | no such configuration |
| WASAPI exclusive / IAudioClient3 | 2.66 ms | 5.33 ms | no such configuration |
| CoreAudio 32 frames | 0.67 ms | 1.33 ms | no such configuration |

Read honestly: **default against default we are borderline-passing; low-latency
against low-latency we fail by absence.** Our period is fixed at 128 frames and our
pipeline at 8 buffers, negotiated with nothing. Every production server treats both
as client-negotiable, and PipeWire's whole design point is that the quantum is
dynamic. The 46.4 ms client-to-speaker figure is the one to attack first, and it is
attackable without touching the device: it is 8 client slots because `slot_count =
num_buffers`, not because anything requires it.

**Comparability: FAIR.** Buffer geometry is a design constant, not a measurement.
Nothing here depends on TCG. The only TCG-dependent question is whether we could
*sustain* a smaller quantum - that is metric 2's question, not this one.

---

## 3. Metric 2 - wake-up jitter for a real-time audio thread

The metric that matters most, and the one where the literature is richest, because
`cyclictest` has been the standard instrument for twenty years. It measures exactly
one thing: a `clock_nanosleep` is armed for time T at SCHED_FIFO 99; how much later
than T does the thread actually start running.

### 3.1 Primary measurements

**OSPERT 2024, de Wit et al., Raspberry Pi 5, kernel 6.6.21** `[P]`.
Command: `cyclictest -vmn -i100 -p99 -t --duration=1h`, under stress-ng plus an
`iperf3 -s` network load, one hour.

| Kernel | Average | **Maximum** | Std. dev. |
|---|---|---|---|
| Stock (non-RT) | 14.69 us | **36 802 us** | 122.08 us |
| PREEMPT_RT | 5.91 us | **124 us** | 3.25 us |

A 294x reduction in observed maximum. This is the most recent rigorous number found
and the one to treat as the modern reference for "a well-configured RT system under
heavy load", with the condition that it is ARM Cortex-A76, not x86.

**OSPERT 2013, Brandenburg and Gul, 16-core Intel Xeon X7550** `[P]`.
`cyclictest` at SCHED_FIFO 99, one thread per core, `-i1000 -d500`, 20 min per
config, ~5.85M samples each. Frequency scaling and deep sleep states disabled.

| Background load | Linux 3.0 max | Linux 3.8.13 max | 3.8.13 + PREEMPT_RT max | RT avg / median |
|---|---|---|---|---|
| Idle | 13.89 us | 19.73 us | **11.20 us** | 2.74 / 2.57 us |
| CPU-bound (20 MiB working set per core) | 72.73 us | 64.47 us | **17.42 us** | 3.40 / 3.02 us |
| I/O-bound (hackbench + bonnie++ direct I/O + 16x wget) | 4300 us | **5464 us** | **44.16 us** | 4.12 / 4.07 us |
| bonnie++ on every core | 80-200 **ms** spikes | ditto | **< 50 us** | - |

Two things this table establishes that no single number can. First, mainline Linux's
scheduling latency is *load-shaped*: 20 us idle, 65 us under CPU load, 5.5 ms under
I/O load, 200 ms under a disk storm. Second, PREEMPT_RT's whole value is that the
distribution stops caring: median moves 2.57 -> 4.07 us across four orders of
magnitude of background stress.

**ECRTS 2020, Bristot de Oliveira et al., Intel i7-6700K @ 4.00 GHz, kernel-rt
5.2.21-rt14, Fedora 31, tuned to RT best practice** `[P]`.
`perf script record rtsl cyclictest --smp -p95 -m -q`, 30 min per workload.

- Observed `cyclictest` latency: **27 us**.
- Analytical interference-free bound from the same trace: 42.2 us.
- Sliding-window bound with observed IRQ arrival patterns: 98 us.
- Longer runs / heavier load raise the *bound* (not the observation) to 467 us and
  801 us; on a 2-socket NUMA Xeon L5640 server the bound reaches 1900-2944 us.
- The paper's own survey sentence, worth quoting because it is the field's
  consensus: "Maximum observed latency values generally range from a few
  microseconds on single-CPU systems to **250 microseconds** on non-uniform memory
  access systems, which are acceptable values for a vast range of applications with
  sub-millisecond timing precision requirements."

**OSADL long-term monitoring, Emde** `[P]`. Older hardware, but it is the only source
found that publishes the exact command lines alongside the maxima, and OSADL has run
this continuously on ~180 systems since.

| System | cyclictest command | Samples | Max |
|---|---|---|---|
| AMD Athlon 64 2800+, uniprocessor, 2.6.33.7-rt29 | `cyclictest -l100000000 -m -a0 -t1 -n -p99 -i200 -h200 -q` | 100 M | **50 us** |
| Intel i7 Nehalem 975 @3333 MHz, 8-way, same kernel | `cyclictest -l100000000 -m -Sp99 -d0 -i200 -h200 -q` | 100 M | **34 us** |

In-kernel continuous monitoring on the same machines: wakeup-latency max 25.58 us
idle; missed timer offsets up to 59.26 us under load; combined timer-and-wakeup
19.98 us (best core) to 36.28 us (worst core). OSADL's farm methodology `[P]`:
5 kHz cyclic timer interrupts, 5 h 33 min per measurement period, twice daily, 100
million cycles per core, 1 us histogram resolution.

### 3.2 Configuration dependence, stated plainly

Every number above moves with:

- **Preemption model.** PREEMPT_RT is now in mainline as of Linux 6.12 (LWN, "The
  realtime preemption end game - for real this time") `[P]`, but it is not the
  default: a stock 6.12 build is `PREEMPT_DYNAMIC`. One user-reported measurement on
  6.12.7 gives max 5340 us and 753 us on two runs, 240 us with `preempt=full`, and
  65 us on a 6.10-rt build `[S]` - consistent in shape with the peer-reviewed
  numbers, and cited only to show the mainline-versus-RT gap has not closed.
- **Tuning.** Frequency scaling, C-states, SMIs, IRQ affinity, `isolcpus`,
  `rcu_nocbs`. The ECRTS and OSPERT papers both disable or pin these; an untuned
  machine is worse by a large factor.
- **Load.** See the 2013 table. It is the single largest term.
- **Topology.** NUMA costs an order of magnitude in the analytical bound.

### 3.3 The 2x line, and ToyOS against it

Consensus reference band, tuned PREEMPT_RT under load: **maximum wake latency
20-130 us**, with 250 us as the field's stated upper end for NUMA. Take 124 us (the
modern, one-hour, heavily-loaded number) as the reference. **2x line = 248 us.**

ToyOS's `max_wake_lat_us`, recorded over 30 runs per config in
`tests/audio-baseline.toml`: median **7 616-9 418 us**, worst single window **56 909
us**.

That is ~38x the 2x line on the median and ~460x on the maximum. Our *median* is
roughly the *maximum* that mainline non-RT Linux shows under a CPU-bound load.

**Comparability: NOT COMPARABLE. Three independent reasons, any one of which is
sufficient.**

1. **It is not the same metric.** `cyclictest` measures programmed-wakeup to
   thread-start, entirely inside one machine, with no device in the loop. soundd's
   counter measures "the DLL predicted a DMA completion at T; how much later than T
   did the mix loop get control" - which folds in the emulated device's completion
   timing. CLAUDE.md records QEMU handing back the entire 23.2 ms pipeline in
   0.7-6.6 ms bursts at stream start, up to 34x real time. On a host-timed audio
   backend, device-model jitter is plausibly the dominant term, and no part of that
   is a scheduler measurement.
2. **TCG distortion lands squarely on the wake path and is non-uniform.** Arming the
   LAPIC one-shot timer is three x2APIC `wrmsr`, each a device-model exit under TCG
   (`specs/cpu-attribution.md`). The address-space switch is 6.5x. The ALU work in
   between is 1.06x. Device MMIO is 0.19x - TCG is *faster* than a hypervisor there.
   A path built from components spanning 0.19x to 6.5x has no recoverable scale
   factor.
3. **The kernel is unoptimized.** opt-level 0, debug-assertions, overflow-checks, and
   never once built release in this tree. `clock::nanos_since_boot` calls `rdtsc`
   out of line despite `#[inline]`. This inflation is not TCG and holds on hardware.

**What would make it comparable.** Write `cyclictest` for ToyOS: a userspace thread
at RT priority that arms an absolute timer, sleeps, and histograms
(actual - programmed) with the same 1 us resolution OSADL uses. That removes reason 1
entirely and turns reasons 2 and 3 into a quantifiable, falsifiable prediction (build
release; measure the delta). Until such a tool exists, **no honest 2x claim can be
made on this metric in either direction**, and the current number should not be cited
as evidence that ToyOS's scheduler is 500x worse than Linux's - it is not evidence of
that, because it is not measuring that.

---

## 4. Metric 3 - idle CPU cost of an audio server with no clients

The metric the owner expected us to fail. We do, and the failure is categorical
rather than quantitative, which makes it the easiest one to fix.

### 4.1 What production does: suspend the device on idle

This is universal, and it is the single most useful finding in this document.

**PulseAudio.** `module-suspend-on-idle` "suspends devices when no streams are
connected to them" `[P freedesktop.org PulseAudio Modules]`. Default timeout **5
seconds** `[P, corroborated by module source and debug logs]`; suspending closes the
underlying ALSA device. A negative timeout disables suspension.

**PipeWire / WirePlumber.** `session.suspend-timeout-seconds` - "By default this is
`5` seconds" `[P WirePlumber ALSA configuration]`. The proof that the default
*releases* the hardware is in the same document's description of the escape hatch: a
value of 0 "disables suspend for a node and will leave the ALSA device busy". A
sibling knob, `node.pause-on-idle`, defaults to false, and the documented reason is
telling: "because some devices make a 'pop' sound when they are opened/closed".
PipeWire's author lists "Suspend of idle devices" as a *session manager*
responsibility, not a server one (FOSDEM 2019, slide 22) `[P]`.

**Windows.** Microsoft frames buffer size as directly a power decision `[P MS Learn]`:
"If the system uses 10-ms buffers, it means that the CPU will wake up every 10 ms,
fill the data buffer and go to sleep. However, if the system uses 1-ms buffers, it
means that the CPU will wake up every 1 ms... the CPU will wake up more often and the
power consumption will increase. This will decrease battery life." And the low-latency
resource-isolation mode is entered and left with streaming: "When the application
stops streaming, Windows returns to its normal execution mode."

**macOS.** The device's I/O cycle exists only between `AudioDeviceStart` and
`AudioDeviceStop` for a registered `AudioDeviceIOProcID` `[P AudioHardware.h]`: an
AudioDevice "provides a single IO cycle, a timing source based on it, and all the
buffers synchronized to it". With no started IOProc there is no cycle, hence no
wakeups. Apple's power guidance never mentions idle cost because idle cost is zero by
construction; the whole of TN2321 is about the *streaming* case: "The size of the I/O
buffer controls the rate at which the audio stack will wake the CPU in order to
perform I/O. Therefore, the I/O buffer size will nearly always be the dominant factor
affecting audio stack power usage."

### 4.2 The second-level technique: size the period to the client, not to the hardware

Even before suspend, PulseAudio's "glitch-free" rework changed the shape of the
problem, and Lennart Poettering's write-up is the primary source `[P
0pointer.de/blog/projects/pulse-glitch-free.html]`:

- Configure the hardware buffer as large as possible, "up to 2s", and "configure a
  system timer to wake us up 10ms before the buffer would run empty".
- "only partially fill the buffer each time we wake up", sized by connected clients'
  latency requirements.
- Before: "a fragment size of 25ms by default, with four fragments", giving "40
  interrupts/s"; dmix generated "at least 47 interrupts/s".
- After: "minimize the overall number of interrupts, down to what the latency
  requirements of the connected clients allow us."
- With no client asking for anything: "fill up the whole buffer all the time, i.e.
  have an actual latency of 2s" - roughly **0.5 wakeups per second**.

PipeWire generalizes this to a dynamic quantum negotiated across the graph; the
`min-quantum`/`max-quantum` pair (32 and 8192 by default `[P]`) is exactly the range
within which the server is free to move.

So production's idle story is two-level: **dynamic period sized to demand, and full
device suspend when demand is zero.** The 5-second timeout on both Linux servers is
the amortization constant for the pop-and-re-prime cost that `node.pause-on-idle`'s
default documents.

### 4.3 The 2x line, and ToyOS against it

| System | Idle cost, no client streaming |
|---|---|
| PulseAudio, default modules | device suspended after 5 s; **0 wakeups, 0% CPU** |
| PipeWire + WirePlumber, defaults | node suspended after 5 s; **0 wakeups, 0% CPU** |
| CoreAudio | no started IOProc, no I/O cycle; **0 wakeups** |
| Windows audio engine | not streaming; **normal execution mode**, no periodic fill |
| PulseAudio, suspend disabled (worst production case) | 2 s buffer, **~0.5 wakeups/s**, silence |
| **ToyOS soundd** | **~43 wakeups/s, ~7% of a core**, mixing and dithering silence |

**2x line = 0%. ToyOS fails categorically.** There is no ratio to compute: production
is structurally zero, and any positive number is unboundedly over.

The nearest thing to a fair comparison is against PulseAudio with suspend explicitly
disabled - the worst production configuration anyone actually runs - and even there we
are ~86x the wakeup rate and doing real per-sample work (mix, dither, quantize) on
every one of them where PulseAudio writes a pre-filled buffer.

### 4.4 The design fix, in ToyOS's terms

The standard technique adopted:

1. **Stop submitting periods when no stream is `is_streaming()`.** soundd already has
   this predicate - `f25fa87` introduced it so `underruns` would stop counting
   connect-time pre-roll. The mix loop can use the same latch.
2. **Release the device after a timeout.** Stop the virtio-sound PCM stream and let
   the completion IRQ stream go quiet. Five seconds is the production constant on both
   Linux servers and is not arbitrary: it amortizes the open/close artifact.
3. **Re-prime on the next `MSG_STREAM_OPEN`.** This is the dangerous half, and our own
   history says so: the re-prime path was the dominant glitch source (mode A,
   `specs/audio-glitch-distribution-2026-07-28.md`), and PipeWire defaults
   `node.pause-on-idle` to false precisely because open/close pops. Suspend-on-idle
   must land together with a correct start ramp, not before it.
4. **Put the policy where production puts it.** PipeWire's session manager owns the
   suspend decision; PulseAudio's is a loadable module. soundd owning the device
   unconditionally with a hard-coded timeout would work but repeats a structure both
   projects deliberately moved out of the server.

**Comparability: FAIR - the most comparable metric in this document.** "Does the
server keep the device streaming when nobody is playing?" is a yes/no architectural
question that TCG cannot change. The *magnitude* of the 7% is inflated by TCG and by
the opt-level-0 kernel and should not be quoted as a hardware number; the comparison
does not need the magnitude, because the reference is zero.

---

## 5. Metric 4 - context switch and cross-CPU wake latency

These numbers are notoriously method-dependent, and the honest position is that the
spread between published figures is mostly a spread between definitions.

### 5.1 Same-core context switch

- **Bendersky 2018** `[S, but the method is fully documented]`. Haswell i7-4771,
  16 GB. Two methods: pipe ping-pong between threads, and condition-variable
  signalling. Result: "somewhere between **1.2 and 1.5 microseconds** per context
  switch" when both threads are pinned to one core; "the switch time goes up to
  **~2.2 microseconds**" unpinned. Explicitly *direct* cost only - it excludes the
  indirect cost of a cold cache after switching between threads with different
  working sets, which in real workloads can dominate. Go goroutines, for contrast,
  ~170 ns with no kernel switch at all.
- **`perf bench sched pipe`** `[P man7 perf-bench(1)]`. 1 000 000 pipe operations
  between two tasks, reports usecs/op and ops/sec. The man page's own example output
  is 5.855 usecs/op / 170 792 ops/sec. Treat this as an *upper* bound on the switch:
  each op is two context switches plus two syscalls plus a pipe copy, and the figure
  swings by 3x across machines (a second published example in the same family shows
  16.948 usecs/op).
- **OSPERT 2013's medians** `[P]` are the closest thing to a wake-to-running figure
  with a stated method: 2.57-3.10 us median on a 2010-era Xeon, including the timer
  interrupt. That bounds a full wake path, not just the switch.

**Reference band: 1.2-2.2 us for a same-core switch, direct cost, pinned/unpinned.
2x line = 2.4-4.4 us.**

### 5.2 Cross-CPU wakeup (IPI to running)

This is where the literature is thinnest and I will not invent a number.

- OSPERT 2013 notes that IPIs "are not delivered and processed instantaneously in
  real systems and affect scheduling latency", and that on x86 with processor-local
  APIC timers a *timer* wakeup needs no IPI at all - which is why `cyclictest`
  numbers do not contain one. A cross-CPU *task* wakeup does.
- **No primary published `smp_call_function_single` round-trip measurement was
  found.** Figures in the 1-3 us range circulate; none of them traced back to a
  primary source in this search. Treat any such number as unsourced.
- `schbench` (Chris Mason) is the right instrument - it measures precisely
  wakeup-to-execution latency percentiles under a message/worker workload. Published
  numbers span 99.0th percentile 8 us / 99.9th 14 us at light load to 272 us / 1266 us
  on a loaded 90-second run `[S]`. The spread is the point: this statistic is a
  function of load, not of the machine.

### 5.3 ToyOS against it

**We do not have the number.** No end-to-end context-switch or wake measurement exists
in this tree. The only adjacent measurement is
`specs/arm64-research-2026-07-28.md`'s address-space switch + TLB flush: **1619 ns
under x86-64 TCG, 251 ns native** - and the native leg was measured on aarch64 under
HVF, a different ISA. That is one component of a switch, measured on two different
architectures.

**Comparability: NOT COMPARABLE, and not measurable on this host.** Even a correctly
written ping-pong benchmark - which would be directly comparable *in method* to
`perf bench sched pipe` - would produce a TCG number whose ratio to bare metal is
unknown and provably not a single constant (the switch's address-space half is 6.5x,
its ALU half 1.06x).

**Recommendation: do not attempt a 2x claim on scheduler micro-latency until either
an ARM64/HVF port or an x86-64 host with KVM exists.** Until then the only sound
scheduler gate is the relative one already in place: same host, same session, A/B
against the same HEAD, which is exactly what CLAUDE.md's workflow rules already
require and what the Stage 7a bisection actually used.

---

## 6. Metric 5 - architecture: where we are structurally better or worse

### 6.1 Production audio servers are pull/graph-driven. We are not.

PipeWire `[P docs.pipewire.org, Graph Scheduling]`: "The graph can only run if there
is a driver node that is in some way linked to an active node." The driver "will use a
timer or some sort of interrupt from hardware to start the cycle", then sets pending
counters on each follower and atomically decrements each target's `required` field;
when it reaches zero the follower's eventfd is signalled. PipeWire's author states the
audio model directly (FOSDEM 2019, slide 29) `[P]`: "Pro Audio model like JACK is
chosen... **All nodes are woken up in each cycle in turn**... Sinks have an audio
adapter in front to mix, merge, resample, split and convert the channels."

JACK is the same model by construction: the server calls each client's `process()`
once per period.

ToyOS's soundd is not this. Clients fill a per-client shared-memory slot ring
asynchronously and soundd consumes from it. That is closer to PulseAudio's model than
to JACK's - and PulseAudio's model is the one that needs rewind support and a
glitch-free scheduler to be tolerable.

**The consequence is our recorded defect class.** In a driver-woken graph a client
cannot be "mid-refill" when the server mixes, because the server *calls* the client
and waits. The stream-start dropout bisected to `f4d8fa7` - "one mix cycle drains all
8 client slots and the client then has a single signal->mix window to regenerate 8
periods" - is structurally impossible in JACK or in PipeWire's pro-audio path. There
is no ring to drain: there is one buffer per cycle.

**Nuance, and it is the useful one.** PipeWire is *both*. Its "audio stream" adapter
(slide 32) `[P]` "Takes input from client (asynchronously)... Decouples server buffer
size from client requested latency". So PipeWire runs a synchronous graph for pro
clients and an asynchronous adapter for everyone else - and the adapter exists
precisely to absorb the failure mode we hit. If we keep the async model, the adapter's
job (decoupling, and absorbing a client that is late by less than its ring depth) is
the thing to copy. PipeWire also has "lazy scheduling" via `RequestProcess` `[P docs]`
for the case where the follower's rate is the better clock - worth knowing about
before designing anything here.

### 6.2 Nobody drains a client's whole ring in one cycle

Because in a graph model there is no ring. One quantum per node per cycle. The
"drain the whole client ring" shape has no analogue in JACK, PipeWire's pro path,
CoreAudio's IOProc (one buffer per cycle), or WASAPI's engine period. Our fix
(`069d158`, defer a buffer when the client is mid-refill, gated on
`refill_floor_nanos = 5 * period_nanos`) is a repair to a structure production servers
do not have. It works, and the baseline says so - 0 dropouts in 120 config-runs - but
it should be understood as compensating for an architectural choice rather than as a
feature.

### 6.3 A DLL clock-recovery loop is standard practice, not unusual

Three independent confirmations, all primary:

- **JACK documents its DLL in the public API** `[P jackaudio.org, Time Functions]`:
  "The value of period_usecs will in general NOT be exactly equal to the difference of
  next_usecs and current_usecs. This is because to ensure stability of the DLL and
  continuity of the mapping, a fraction of the loop error must be included in
  next_usecs."
- **PipeWire ships one as a core utility**: `spa/include/spa/utils/dll.h`, by Wim
  Taymans, MIT-licensed, with `SPA_DLL_BW_MAX = 0.128` and `SPA_DLL_BW_MIN = 0.016`
  `[P]`. The author's own FOSDEM 2019 changelog lists "DLL for resampling and audio
  timing in devices" as a landed feature `[P]`.
- Both descend from Fons Adriaensen, *Using a DLL to filter time* (2005) `[P]`, the
  paper that introduced the technique to Linux audio.

**ToyOS's `Dll` uses `bw = 0.03`, which sits inside PipeWire's [0.016, 0.128] band.**
This part of our design is conventional and correctly parameterised. It is worth
recording explicitly because the DLL is the component most likely to be second-guessed
by someone who has not seen that JACK and PipeWire both ship one.

### 6.4 RT priority is a privileged, structured mechanism everywhere except here

- **Windows**: applications are told not to create their own threads but to submit
  work to the Real-Time Work Queue tagged "Audio" or "ProAudio"; drivers must
  *register* their interrupts and threads with Portcls so the OS can protect them; and
  the isolation mode is entered only when small buffers are requested `[P MS Learn]`.
- **macOS**: `thread_policy_set` with `THREAD_TIME_CONSTRAINT_POLICY` (period,
  computation, constraint, preemptible), plus **audio workgroups** - `os_workgroup_join`
  on the device's workgroup so the kernel knows which threads share one deadline
  `[P Apple developer documentation]`. Registering threads with a workgroup "helps the
  system direct work to the right system resources".
- **Linux**: `RLIMIT_RTPRIO` and rtkit gate `SCHED_FIFO`.
- **ToyOS**: `SYS_SET_RT_PRIORITY` is completely ungated (CLAUDE.md, soundd audit
  defect 3). Any process can put any number of threads in the RT band and starve
  soundd's mix thread.

That is recorded as a security defect and it is one, but the architectural reading is
sharper: **we have the priority but not the mechanism.** Every production system has a
way for the kernel to know *which* threads belong to the audio deadline, and uses it
both to protect them and to decide co-scheduling. Audio workgroups in particular have
no ToyOS analogue and would be needed the first time a client has more than one
rendering thread.

### 6.5 Is anything of ours structurally better?

Nothing found. The closest candidate is that our device pipeline is fixed and shallow
(8 x 2.902 ms) where PulseAudio's default is a 2-second hardware buffer with
timer-based partial fills. Ours is simpler and puts a hard bound on how stale queued
audio can be. But production made the opposite trade deliberately and then bought back
the responsiveness with rewind support - PulseAudio's "zero-latency rewrites" exist
because a big buffer would otherwise make a volume change laggy `[P 0pointer.de]`. So
the honest verdict is **simpler, not better**: we have neither the dynamic latency nor
the power story, and we pay for the simplicity in metric 3.

---

## 7. Summary: the 2x line, per metric

| # | Metric | Production reference (config stated) | 2x line | ToyOS | Verdict | Comparable today? |
|---|---|---|---|---|---|---|
| 1a | Output latency, default config | CoreAudio 512 fr @44.1k = 11.6 ms; WASAPI shared 11.3 ms; PipeWire 1024/48k = 21.3 ms; JACK 1024x2/48k = 42.7 ms | 22.6-85.3 ms | 23.2 ms server->device | Borderline pass | **Yes** |
| 1b | Output latency, client->speaker | same | same | up to 46.4 ms (8 device + 8 client periods) | Fail vs the two tightest defaults | **Yes** |
| 1c | Output latency, low-latency config | PipeWire 0.67 ms; WASAPI/JACK 2.67 ms; CoreAudio 0.73 ms | 1.3-5.3 ms | no such configuration exists | **Fail by absence** | **Yes** |
| 2 | RT thread wake jitter, max | PREEMPT_RT 124 us (Pi 5, 1 h, heavy load); 27 us (tuned i7-6700K); 44 us (Xeon, I/O storm); field consensus <= 250 us | ~250 us | median 7.6-9.4 ms, max 56.9 ms | Nominally ~40x/~460x over | **NO** - different metric, TCG, unoptimized kernel |
| 3 | Idle CPU, no clients | 0% and 0 wakeups (device suspended after 5 s: PulseAudio, PipeWire, CoreAudio, WASAPI) | 0% | ~7% of a core, ~43 wakes/s | **Fail categorically** | **Yes** - it is a yes/no |
| 4a | Same-core context switch | 1.2-1.5 us pinned, ~2.2 us unpinned (Haswell, direct cost) | 2.4-4.4 us | never measured | Unknown | **NO** - and not measurable on this host |
| 4b | Cross-CPU wake (IPI -> running) | no primary source found; schbench 99.9th spans 14 us to 1.3 ms with load | n/a | never measured | Unknown | **NO** |

### What to do with this, in priority order

1. **Fix metric 3.** It is the only outright architectural failure, the fix is
   documented practice (suspend the device after ~5 s idle; stop mixing silence), and
   it is unaffected by every measurement caveat in this file. Land it behind a correct
   re-prime ramp, because our own glitch history and PipeWire's `node.pause-on-idle`
   default both say the open/close transition is where this bites.
2. **Attack metric 1b before 1a.** 46.4 ms client-to-speaker is 8 client slots because
   `slot_count = num_buffers`, not because anything requires it. Halving the client
   ring is a smaller change than touching the device pipeline and moves us from
   "fails the two tightest defaults" to "passes all four".
3. **Build a `cyclictest` for ToyOS before making any claim about metric 2.** Arm an
   absolute timer at RT priority, histogram (actual - programmed) at 1 us resolution.
   It costs little, it removes the device from the loop, and it turns the TCG question
   into a measurable delta rather than an argument.
4. **Do not claim a scheduler ratio at all until KVM or HVF exists.** Keep gating on
   same-host A/B, which is what the tree already does and what the Stage 7a bisection
   actually relied on.
5. **Copy the mechanism, not just the priority, for RT threads.** Gate
   `SYS_SET_RT_PRIORITY`, and when clients grow a second rendering thread, look at
   Apple's audio workgroups rather than inventing something.

---

## 8. Sources

Primary `[P]`:

- PipeWire, `pipewire.conf(5)` - https://docs.pipewire.org/page_man_pipewire_conf_5.html
- PipeWire, `pipewire-props(7)` - https://docs.pipewire.org/page_man_pipewire-props_7.html
- PipeWire, Graph Scheduling - https://docs.pipewire.org/page_scheduling.html
- PipeWire, `spa/include/spa/utils/dll.h` - https://raw.githubusercontent.com/PipeWire/pipewire/master/spa/include/spa/utils/dll.h
- Wim Taymans, *PipeWire*, FOSDEM 2019 slides - https://archive.fosdem.org/2019/schedule/event/pipewire/attachments/slides/2826/export/events/attachments/pipewire/slides/2826/PipeWire.pdf
- WirePlumber, ALSA configuration - https://pipewire.pages.freedesktop.org/wireplumber/daemon/configuration/alsa.html
- `jackd(1)` - https://man.archlinux.org/man/jackd.1.en
- JACK API, Time Functions - https://jackaudio.org/api/group__TimeFunctions.html
- Fons Adriaensen, *Using a DLL to filter time* - https://kokkinizita.linuxaudio.org/papers/usingdll.pdf
- PulseAudio Modules (module-suspend-on-idle) - https://www.freedesktop.org/wiki/Software/PulseAudio/Documentation/User/Modules/
- Lennart Poettering, *Glitch-Free PulseAudio* - https://0pointer.de/blog/projects/pulse-glitch-free.html
- Microsoft, *Low Latency Audio* - https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/low-latency-audio
- Apple, TN2321 *Saving Power During Audio I/O* - https://developer.apple.com/library/archive/technotes/tn2321/_index.html
- Apple, `AudioHardware.h` (AudioDeviceStart/Stop, buffer frame size) - https://raw.githubusercontent.com/phracker/MacOSX-SDKs/master/MacOSX10.13.sdk/System/Library/Frameworks/CoreAudio.framework/Versions/A/Headers/AudioHardware.h
- Apple, Workgroup Management / `os_workgroup_join` - https://developer.apple.com/documentation/audiotoolbox/workgroup-management
- Apple, `thread_policy_set` - https://developer.apple.com/documentation/kernel/1418892-thread_policy_set
- de Wit et al., *A Preliminary Assessment of the Real-Time Capabilities of Real-Time Linux on Raspberry Pi 5*, OSPERT 2024 - https://antonio.paolillo.be/publications/workshops/ecrtsOspert2024_dewit_rtlinux_paper.pdf
- Bristot de Oliveira, Casini, de Oliveira, Cucinotta, *Demystifying the Real-Time Linux Scheduling Latency*, ECRTS 2020 - https://retis.santannapisa.it/~tommaso/publications/ECRTS-2020.pdf
- Brandenburg, Gul, *A Comparison of Scheduling Latency in Linux, PREEMPT RT, and LITMUS^RT*, OSPERT 2013 - https://people.mpi-sws.org/~bbb/papers/pdf/ospert13.pdf
- Emde, *Long-term monitoring of apparent latency in PREEMPT RT Linux real-time systems*, OSADL - https://www.osadl.org/fileadmin/dam/articles/Long-term-latency-monitoring.pdf
- OSADL QA Farm description - https://www.osadl.org/fileadmin/dam/documents-public/OSADL-QA-Farm-Description.pdf
- OSADL latency plots (live) - https://www.osadl.org/Latency-plots.latency-plots.0.html
- LWN, *The realtime preemption end game - for real this time* (PREEMPT_RT in 6.12) - https://lwn.net/Articles/989212/
- `perf-bench(1)` - https://man7.org/linux/man-pages/man1/perf-bench.1.html

Secondary `[S]`:

- Eli Bendersky, *Measuring context switching and memory overheads for Linux threads* (2018) - https://eli.thegreenplace.net/2018/measuring-context-switching-and-memory-overheads-for-linux-threads/ - method fully documented, hardware stated; classed secondary only because it is a blog.
- masoncl/schbench - https://github.com/masoncl/schbench - tool is primary; the percentile figures quoted circulate via LWN's scheduler-benchmark survey and downstream wikis and were not traced to a single primary run.
- Wim Taymans, AGL/ALS 2019 PipeWire deck (the 0.7%/2.3%/2.7%/6% CPU figures for a 24-bit 96 kHz 5.1 downmix at 21.33 ms and 1.33 ms on an 800 MHz core). The host returns HTTP 403 to automated fetches; the numbers here come from search-result summaries and are **not** relied on by any conclusion.
- Forum-reported cyclictest on kernel 6.12.7 PREEMPT_DYNAMIC (5340 / 753 us; 240 us with `preempt=full`; 65 us on 6.10-rt). Cited only to show the mainline-vs-RT gap persists on current kernels; the shape agrees with the peer-reviewed sources.

Internal, for the ToyOS side:

- `tests/audio-baseline.toml` - the recorded 30-run sample per config.
- `specs/arm64-research-2026-07-28.md` - TCG vs HVF microbenchmarks measured on this host.
- `specs/cpu-attribution.md` - the unoptimized-kernel finding and the TCG analysis.
- `specs/audio-glitch-distribution-2026-07-28.md` - the re-prime glitch mode.
- `kernel/src/drivers/virtio_sound.rs`, `userland/soundd/src/main.rs` - the geometry.
