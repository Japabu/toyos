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
| Storage | inspect the disk image host-side after shutdown | **nothing** |
| xHCI | QMP `device_add` / `device_del` | **nothing** |

## Priority: device *shape and lifecycle* before *protocol depth*

Every driver defect this project has found came from changing **what devices
exist**, not from asserting harder against one configuration:

- the boot-fatal xHCI panic — from removing HID (M1)
- USB hotplug silently broken — from adding a device after boot
- the 3-slot panic that will kill the first T14 boot — from having four devices
- three daemons panicking — from removing audio and the NIC (M1)
- the audio dropout — from load

A shape matrix is also the cheapest thing to build: it is boot configurations,
not new instruments. Five configs beyond metal-sim — no-USB-HID,
four-USB-devices, no-NVMe, hotplug-after-boot, remove-under-load — cost roughly
3–5 s per boot, so about 25 s on a suite that runs in 70. That buys the class of
bug that has produced every metal-track blocker so far.

Protocol depth comes second, and only where the device is load-bearing: storage
(data loss is unrecoverable) and network (gate N).

## Build order

1. **Shape matrix.** Parameterize the existing profile machinery over device
   sets; assert the boot reaches the compositor and that the right daemons live
   or exit. A daemon whose device is absent must exit, not panic and not hold
   its service name with nothing behind it.
2. **Lifecycle.** `device_add`/`device_del` mid-run: hotplug, removal under
   active I/O, claim-then-die-then-reclaim.
3. **Storage ground truth.** Write files in-guest, shut down, mount the image
   host-side, verify bytes. Nothing today proves an NVMe write ever lands.
4. **Gate N.** Already specified.

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
