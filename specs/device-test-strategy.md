# Device and driver testing strategy

How ToyOS tests the code that talks to hardware. Gate A (audio) and gate N
(`specs/net-gate-plan.md`) are instances of this; this file is the general rule
and the build order.

## Two principles, both earned

**1. Ground truth lives at the hardware boundary, never in the guest's own
report.** Gate A works because the wav capture is what the *device* received:
soundd's counters could all lie and the gap detector would still fire. A test
that asks the guest whether the guest is well tests nothing that matters.

**2. The harness is an actuator, not only an observer.** QMP can add and remove
devices at runtime, inject keystrokes and mouse packets, and impair links.
Anything we can only observe, we can only test on the happy path.

Boundary per device class, and what exists today:

| Class | Ground truth | State |
|---|---|---|
| Audio | `-audiodev wav` capture | built (gate A) |
| Display | QMP screendump + exact glyph decode | built (M0/M1) |
| Input | QMP key/mouse injection | built (M2) |
| Network | `filter-dump` pcap + harness-as-peer | planned (gate N) |
| Storage | inspect the disk image host-side after shutdown | shape built (`MetalDisk`), depth partial |
| xHCI | boot-time device set, then QMP `device_add` / `device_del` | shape built (`MetalUsb`), lifecycle **nothing** |

## Priority: device *shape and lifecycle* before *protocol depth*

Every driver defect this project has found came from changing **what devices
exist**, not from asserting harder against one configuration:

- the boot-fatal xHCI panic — from removing HID (M1)
- USB hotplug silently broken — from adding a device after boot
- the 3-slot panic that would have killed the first T14 boot, and the interrupt
  ring two keyboards shared — both from having six devices (`MetalUsb`)
- three daemons panicking — from removing audio and the NIC (M1)
- the audio dropout — from load
- the page cache's device-sized index, which killed the first boot on the T14 —
  from the disk being 244 GB instead of 128 MB

**A device's *size* is part of its shape, and it is the dimension this list
missed.** Every config above varies which devices exist; none varied how big one
is, and a kernel structure sized per device block is invisible on a 128 MB test
disk and fatal on a 244 GB laptop one. `Profile::MetalDisk` closes it at zero
cost: `File::set_len` gives a sparse image, so the guest sees the T14's exact
namespace and the host spends 7.5 MB. Any device with a capacity — and that is
most of them — should be asked for the real number rather than a token one.

A shape matrix is also the cheapest thing to build: it is boot configurations,
not new instruments. Five configs beyond metal-sim — no-USB-HID,
four-USB-devices, no-NVMe, hotplug-after-boot, remove-under-load — cost roughly
3–5 s per boot, so about 25 s on a suite that runs in 70. That buys the class of
bug that has produced every metal-track blocker so far.

The crowded-USB config is built: `Profile::MetalUsb` is metal-sim with six
devices on the xHCI, two of them keyboards, and it cost two boots, one extra
kernel build and no new instrument. The extra build is the shortage config's:
a distinct `kernel_features` set is a distinct cargo fingerprint, so budget one
rebuild per feature-carrying config, not one boot. It also fixed the shape of
the assertion — the driver logs the DMA offset of each device's interrupt ring
and the block count it derived, so "these two devices are independent" and "this
number came from HCSPARAMS" are both text assertions rather than hopes. One
caveat it recorded: a *shortage*
scenario is not always host-stageable. QEMU's `nec-usb-xhci,slots=N` does not
reach HCSPARAMS1 and its Enable Slot ignores the MaxSlotsEn the driver writes,
so the exhaustion path needs a kernel feature (`xhci-one-slot`) as its
actuator. Check that the actuator exists before promising the config.

Protocol depth comes second, and only where the device is load-bearing: storage
(data loss is unrecoverable) and network (gate N).

## How a test observes: text first, pixels only as a last resort

Owner's rules, 2026-07-31. They are rules, not preferences — a test that breaks
for the wrong reason costs more than the coverage it claimed.

**Assert on text, not on pixels.** An in-guest binary that prints what it
received is the default instrument for anything above the hardware boundary.
Pixel-count thresholds and colour-count assertions are banned: they cannot tell
right from wrong-but-changing, which is not hypothetical — the first two
versions of the M2 input test passed vacuously because the compositor repaints
a taskbar clock once a second.

**Never assert on a screen position.** Clicking a fixed coordinate asserts the
compositor's layout, an implementation detail that will change and will then
fail a driver test for an unrelated reason. Input tests inject a *relative*
delta and a button and assert the guest received that delta and that button —
true no matter where anything is drawn.

**The one exception: the panic console.** A screendump is a conversion
taken while the guest is still drawing, and the panic console writes only
`0x00` and `0xFF` — so any other colour in a band that lost its text names
userland as the last painter, and a missing line is evidence about where on the
glass it sat rather than about what the kernel wrote. There the framebuffer *is* the device
under test, and on a machine with no serial port it is the only diagnostic
channel, so asking the question without looking at pixels is not a weaker test
but no test. Those assertions are text anyway: the harness decodes each 8x16
cell against the same `font8x16.bin` the kernel renders with, so kernel and
decoder cannot drift, and the assertions read `screen_text.contains(...)`. This
exception covers the panic-render tests, their negative control (a *recovering*
panic must not paint — without it "the console paints" can be true while "it
paints when it must not" is also true, which was a real defect), and the
decoder's own host unit test. Nothing else.

**Tests run headless, always.** `-display none` for every profile in the
harness. Manual verification does not get an exemption: a `cargo run` that
opens a window on the owner's desktop is not an acceptable way to demonstrate
a test result.

## Build order

1. **Shape matrix.** Parameterize the existing profile machinery over device
   sets; assert the boot reaches the compositor and that the right daemons live
   or exit. A daemon whose device is absent must exit, not panic and not hold
   its service name with nothing behind it.
2. **Lifecycle.** `device_add`/`device_del` mid-run: hotplug, removal under
   active I/O, claim-then-die-then-reclaim.
3. **Storage ground truth.** Started. `nvme_large_device` writes a file
   in-guest, shuts down, and decodes both superblocks out of the image
   host-side with the kernel's own parser — the clean flag reaches the platter
   only through `PageCache::sync` and the backup only through a write at the far
   end of the device, so one assertion covers write-back and capacity at once.
   What is left is the file's *bytes*: that needs a file-backed `BlockIO` so the
   harness can walk the btree rather than just the superblocks.
4. **Gate N.** Already specified.

## No host stress tests, and no brute-force volume runs

Owner's rule, 2026-07-31. The dev machine is a laptop; multi-million-iteration
loops peg it for minutes and are not how defects in this tree get found.

The evidence, checked rather than assumed:

- **The `toyos-ps2` 10-million-byte fuzz found nothing, and could not have.**
  The M2 review confirmed every one of its assertions was structurally
  unfalsifiable — `usage != 0` enforced by the emitter, the HID range by table
  contents, `buttons < 8` by a mask, `|dx|,|dy| <= 256` by the arithmetic
  domain. The real mouse-framing defect (a body byte with bit 3 set is a legal
  head, so a one-byte misframe self-sustains) was found by *reading the
  framer*, and the host test that passed over it used the one delta pair that
  cannot masquerade as a head.
- **`allocator_stress`, `mmap_stress` and `sched_stress` have never produced a
  fix.** They arrived with the initial import and no commit has touched them
  since.

What does work, and is the exception that proves the rule: the scheduler's
deterministic simulator and interleaving fuzzer (`toyos-sched/sim/`). It is not
volume for its own sake — it explores interleavings against **ownership-typed
invariants**, is reproducible from a seed, and its five negative gates prove it
can still fail. Randomness there is a search strategy over a model with real
assertions, not a substitute for having assertions.

The rule: a test earns its runtime by asserting something that can be false.
If you cannot state what a run would have to observe to go red, more iterations
will not supply it.

### Load the guest, never the host

Host load has never produced a finding here and has repeatedly produced false
ones. It does not stress the system under test — it degrades the instrument:

- two agents building in one target directory produced a rustc ICE
  (`Error writing pre-lto-bitcode file`), first read as a fork bug, actually
  incremental-cache corruption from contention;
- a 30-iteration audio gate run had to be killed at iteration 6 when other
  agents started sharing the machine, rather than record data nobody could
  trust;
- `audio_tone.smp1`'s 54 ms and 57 ms wake-lateness outliers sat in the
  recorded baseline as a suspected scheduler finding for two days; they never
  reappeared in either arm of a quiet-machine A/B, and host suspend is the
  leading explanation.

Guest load is the opposite and has paid repeatedly: `audio_tone_load` runs CPU
burners *inside* ToyOS, is the config where the stream-start dropout showed
worst, and is audio spec §5.10's first-class case. That is load on the subject.

Starving QEMU itself could be a legitimate experiment one day, but it would be
a deliberate isolated one — never background contention during a normal run.

## What not to build

A gate-A-style distributional tier per device. That instrument costs ~17 minutes
and exists because audio is timing-critical and its failures are statistical.
Storage and USB failures are deterministic: a per-run assertion catches them and
a distribution would be expensive noise.

## The instrument is code, and it will be wrong before the driver is

`specs/audio-gate-history.md` records four instrument defects that were read as
properties of the system. Budget for certifying every new observer against a
known-good and a known-bad capture before trusting a single green run, and prove
each new assertion's teeth by breaking what it guards and watching it go red.

Corollary for absence tests: assert on the *mechanism*, not on a name. A profile
that certifies "no USB HID" by grepping argv for `usb-kbd` passes with a
`usb-mouse` attached.

## Configuration fidelity is itself a test surface

QEMU adds default devices — NIC, IDE CD-ROM, parallel port — to any machine that
declares no `-netdev`, and they never appear in argv, so an argv assertion cannot
see them. A profile whose purpose is fidelity must pass `-nodefaults` and be
verified against `query-pci` rather than against its own command line.
