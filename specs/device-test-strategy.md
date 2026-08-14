# Device and driver testing strategy

Rules for testing code that talks to hardware. Gate A (audio) and gate N
(`specs/plans/net-gate-plan.md`) are instances; the staged build-out is
`specs/plans/device-test-plan.md`.

## Ground truth and actuation

1. **Ground truth lives at the hardware boundary, never in the guest's own
   report.** The instrument records what the device received — gate A asserts
   on the `-audiodev wav` capture, so soundd's counters cannot vouch for
   soundd.
2. **The harness is an actuator, not only an observer.** QMP adds and removes
   devices at runtime, injects keystrokes and mouse packets, and impairs
   links. A state the harness can only observe is testable only on the happy
   path.

Ground truth per device class:

| Class | Ground truth |
|---|---|
| Audio | `-audiodev wav` capture |
| Display | QMP screendump + exact glyph decode |
| Input | QMP key/mouse injection |
| Network | `filter-dump` pcap + harness-as-peer |
| Storage | the disk image, inspected host-side after shutdown |
| xHCI | boot-time device set, then QMP `device_add`/`device_del` |

## Shape and lifecycle before protocol depth

Driver coverage varies **which devices exist** — presence, count, size,
hotplug — before it asserts harder against one configuration. A device with a
capacity is given the real number, never a token one: `Profile::MetalDisk`
serves the T14's exact NVMe namespace from a sparse image
(`File::set_len`, 7.5 MB on the host), and `Profile::MetalUsb` puts six
devices on the xHCI, two of them keyboards.

A shape config is a boot configuration, not a new instrument. A shortage
scenario is not always host-stageable: QEMU's `nec-usb-xhci,slots=N` does not
reach HCSPARAMS1, and its Enable Slot ignores the MaxSlotsEn the driver
writes, so slot exhaustion needs the `xhci-one-slot` actuator. Verify the
actuator exists before promising a config.

Protocol depth comes second, and only where the device is load-bearing:
storage (data loss is unrecoverable) and network (gate N).

## Observation rules

**Assert on text, not on pixels.** An in-guest binary that prints what it
received is the default instrument above the hardware boundary. Pixel-count
and colour-count assertions are banned: they cannot tell right from
wrong-but-changing.

**Never assert on a screen position.** A fixed coordinate asserts the
compositor's layout and fails a driver test for an unrelated reason. Input
tests inject a relative delta and a button and assert the guest received that
delta and that button, which holds no matter where anything is drawn.

**The one exception is the panic console.** There the framebuffer is the
device under test and, on a machine with no serial port, the only diagnostic
channel. The panic console writes only `0x00` and `0xFF`, so any other colour
in a band that lost its text names userland as the last painter. The
assertions are still text: the harness decodes each 8x16 cell against the
same `font8x16.bin` the kernel renders with, so kernel and decoder cannot
drift, and the assertions read `screen_text.contains(...)`. The exception
covers the panic-render tests, their negative control (a recovering panic
must not paint), and the decoder's own host unit test. Nothing else.

**Tests run headless, always.** `-display none` for every profile. Manual
verification gets no exemption: a `cargo run` that opens a window on the
owner's desktop does not demonstrate a test result.

## Stress

**No host stress tests and no brute-force volume runs.** A test earns its
runtime by asserting something that can be false: if no observation would
turn a run red, more iterations will not supply one. Randomized exploration
is allowed where it searches interleavings against real invariants, is
reproducible from a seed, and has negative gates proving it can still fail
(`toyos-sched/sim/`).

**Load the guest, never the host.** Host load degrades the instrument, not
the system under test. Guest load is a first-class configuration:
`audio_tone_load` runs CPU burners inside ToyOS (audio spec §5.10).

**No distributional tier per device.** Gate A's exists because audio failures
are statistical. Storage and USB failures are deterministic: a per-run
assertion catches them, and a distribution is expensive noise.

## The instrument is code

Certify every new observer against a known-good and a known-bad capture
before trusting a green run, and prove each new assertion by breaking what it
guards and watching it go red (`specs/assessments/audio-gate-history.md`
records the instrument defects this rule exists for).

An absence test asserts on the mechanism, not on a name: a profile that
certifies "no USB HID" by grepping argv for `usb-kbd` passes with a
`usb-mouse` attached.

QEMU adds default devices — NIC, IDE CD-ROM, parallel port — to a machine
that declares no `-netdev`, and they never appear in argv. A profile whose
purpose is fidelity passes `-nodefaults` and is verified against `query-pci`,
never against its own command line.
