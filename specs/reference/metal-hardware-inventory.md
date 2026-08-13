# The ThinkPad T14 Gen 2: what the silicon is, what ToyOS drives, what waits

The durable answer to three questions a driver author asks about the one real
machine this project targets: **what is in it**, **what does ToyOS drive today**,
and **what is enumerated and skipped**. The third list is the metal roadmap's raw
material and is the reason this file exists.

Everything numeric here — bus/device/function, class codes, vendor:device IDs,
port numbers, speeds, block counts, MHz, GSIs, milliseconds — is transcribed from
the kernel's own log of a boot that happened. The full log is the appendix, so
nothing in the body needs a source that is not in this file. Identifications
("`8086:a0ed` is the PCH xHCI") are a *layer on top* and are marked with where
they came from; where an identification is uncertain it says so rather than
reading confidently.

## Provenance

Two boots on 2026-08-02, same machine, same kernel and bootloader binaries, two
init lists.

| | Boot A | Boot B |
|---|---|---|
| Image | `--diag-boot` (`diag/system.toml`) | `--console-boot` (`console/system.toml`) |
| Init | `/bin/toybox locale --load` (`init_program_len: 25`) | `/bin/console` (`init_program_len: 12`) |
| Initrd | 2,756,608 B | 25,165,824 B |
| Log | appendix below, 160 lines, sha256 `1bf614261cdef978fd8e561bdd082155a808e4f636b31620666c11fc0b911654` | `/log/kernel.log` on the stick, 208 lines, sha256 `00886b516ff5f391c35504d463a84da68d74d0950e4793067487fb59cd7b7802` |

Boot A is the first fully successful diagnostic boot and the primary source. Boot
B is corroboration: every hardware line in it is identical except for timestamps
and the per-boot GUIDs, and it additionally carries an interactive shell session
driven from the laptop's own keyboard. Where the two disagree on a number, both
are given.

Neither log states the machine's total RAM. `specs/plans/metal-boot-plan.md` records
16 GB; these logs do not confirm it. What they do give is the file cache's budget
(64144 pages, 250 MiB on boot A; 64056 pages on boot B), which is derived from
free memory by a policy this file does not reproduce, so it is not a RAM
measurement.

**To refresh this document**: boot `--diag-boot` (photograph the panel) or
`--console-boot` (the log lands in `/log/kernel.log`, and macOS auto-mounts that
partition as `/Volumes/TOYOS-LOG` — that is the entire reason it is a
Microsoft-Basic-Data partition rather than an EFI-typed one).

## At a glance

**Driven today**: NVMe (the controller; the disk itself is refused, see Storage),
both xHCI
controllers, USB mass storage (the boot stick), the i8042 keyboard and its aux
port, the UEFI GOP framebuffer, the I/O APIC, the LAPIC/x2APIC, the HPET, all
8 CPUs, ACPI's MADT/MCFG/HPET/FADT/PM1a.

**Enumerated and skipped**: the Iris Xe iGPU, the HD Audio controller, the
Ethernet controller, the Wi-Fi controller, the SD host controller, both
Thunderbolt NHIs, CSME/HECI, SMBus, the SPI flash controller, the DPTF
participant, the telemetry aggregator, shared SRAM, the eSPI bridge, six PCI
bridges — and on USB, the fingerprint reader, the camera, the smartcard reader
and the Bluetooth radio.

**Not visible to ToyOS at all**: the LPSS I2C controllers the touchpad hangs off.
See "What is not on the PCI bus", which is the single most consequential finding
here for M5.

## CPU, firmware and clocks

| Fact | Value | Line |
|---|---|---|
| MADT CPU list | `[0, 2, 4, 6, 1, 3, 5, 7]` — 8 logical CPUs | `ACPI: MADT cpus=` |
| APIC mode | x2APIC, BSP ID 0 | `LAPIC: x2APIC enabled (ID 0)` |
| BSP features | `smep=on smap=on pcid=on` | `percpu: BSP cpu_id=0` |
| I/O APIC | one unit, `id=2` at `0xfec00000`, `ver=0x20`, GSI 0..119, 120/120 masked at init | `ioapic: id=2` |
| Interrupt source overrides | `0:0->2 edge/high`, `0:9->9 level/high` — nothing covers IRQ 1 or IRQ 12 | `ioapic: iso` |
| TSC | 2419 MHz, period 413328 fs (boot A) / 413314 fs (boot B), calibrated over 50 ms | `TSC:` |
| LAPIC timer | 384032 (A) / 384046 (B) ticks per 10 ms | `LAPIC timer:` |
| HPET | `0xfed00000` | `ACPI: HPET at` |
| RSDP / MCFG / ECAM | `0x94bfe014` / `0x8f428000` / ECAM base `0xc0000000` | `ACPI:` |
| FADT | rev 6, `iapc_boot_arch=0x0011` | `i8042: FADT rev 6` |
| ACPI power | `PM1a=0x1804 SLP_TYPa=7` | `ACPI: PM1a=` |
| UEFI memory map | 2088 bytes handed over | `KernelArgs { memory_map_size: 2088 }` |
| 16550 | absent — loopback reads `0xff` | `serial: 16550 loopback read 0xff` |

Two derived numbers, both arithmetic on the above rather than measurements:

- **87 memory-map entries.** `MemoryMapEntry` is `#[repr(C)]` `{u32, u64, u64}`,
  so 24 bytes; 2088 / 24 = 87. That is the fragmentation M4 is about, on a kernel
  whose PMM takes 2 MiB pages only.
- **The MADT's order puts SMT siblings in the second half.** APIC IDs 0/2/4/6
  come first and 1/3/5/7 after, and the SMP lines follow suit (`cpu1 lapic=2` …
  `cpu4 lapic=1` … `cpu7 lapic=7`). On an i5-1135G7 (4 cores, 8 threads) that
  means ToyOS's `cpu0..cpu3` are on four distinct cores and `cpu4..cpu7` are their
  siblings. **The kernel does not know this** — it reads no CPUID topology leaf
  and the numbering is MADT order, so the property is an artifact of this
  firmware's table, not something the scheduler can rely on.

## PCI: all 24 functions

`PCI: Enumeration complete, 24 functions.` Every row's `bus:dev.fn`, class,
vendor:device and `prog_if` is verbatim. Names come from `pci.ids` version
2026.08.02 (`https://pci-ids.ucw.cz/`) unless the row says otherwise; class names
come from the same file's class section.

| B:D.F | Class | ID | Identification | ToyOS |
|---|---|---|---|---|
| `00:00.0` | `0600` Host bridge | `8086:9a14` | Tiger Lake-UP3/H35 4 cores Host Bridge/DRAM Registers | ignored |
| `00:02.0` | `0300` VGA compatible controller | `8086:9a49` | TigerLake-LP GT2 [Iris Xe Graphics] | **undriven** — display is the firmware GOP |
| `00:04.0` | `1180` Signal processing controller | `8086:9a03` | TigerLake-LP Dynamic Tuning Processor Participant | ignored |
| `00:06.0` | `0604` PCI bridge | `8086:9a09` | 11th Gen Core Processor PCIe Controller | ignored (bridge) |
| `00:07.0` | `0604` PCI bridge | `8086:9a25` | Tiger Lake-LP Thunderbolt 4 PCI Express Root Port #1 | ignored (bridge) |
| `00:07.2` | `0604` PCI bridge | `8086:9a27` | Tiger Lake-LP Thunderbolt 4 PCI Express Root Port #2 | ignored (bridge) |
| `00:0a.0` | `1180` Signal processing controller | `8086:9a0d` | Tigerlake Telemetry Aggregator Driver | ignored |
| `00:0d.0` | `0c03` USB controller, `prog_if=30` XHCI | `8086:9a13` | Tiger Lake-LP Thunderbolt 4 USB Controller | **driven** — 5 ports, nothing connected |
| `00:0d.2` | `0c03` USB controller, `prog_if=40` USB4 Host Interface | `8086:9a1b` | Tiger Lake-LP Thunderbolt 4 NHI #0 | **undriven** |
| `00:0d.3` | `0c03` USB controller, `prog_if=40` USB4 Host Interface | `8086:9a1d` | Tiger Lake-LP Thunderbolt 4 NHI #1 | **undriven** |
| `00:14.0` | `0c03` USB controller, `prog_if=30` XHCI | `8086:a0ed` | 500 Series Chipset Family On-Package USB 3.2 Gen 2x1 (10 Gbs) xHCI Host Controller | **driven** — every internal USB device is here |
| `00:14.2` | `0500` RAM memory | `8086:a0ef` | 500 Series Chipset Family On-Package Shared SRAM | ignored |
| `00:16.0` | `0780` Communication controller | `8086:a0e0` | 500 Series Chipset Family On-Package CSME HECI #1 | **undriven** |
| `00:1c.0` | `0604` PCI bridge | `8086:a0b8` | 500 Series … PCI Express Root Port #1 | ignored (bridge) |
| `00:1c.4` | `0604` PCI bridge | `8086:a0bc` | 500 Series … PCI Express Root Port #5 | ignored (bridge) |
| `00:1c.7` | `0604` PCI bridge | `8086:a0bf` | 500 Series … PCI Express Root Port #8 | ignored (bridge) |
| `00:1f.0` | `0601` ISA bridge | `8086:a082` | 500 Series Chipset Family On-Package eSPI Controller | ignored — but it is what the i8042 is behind |
| `00:1f.3` | `0403` Audio device, `prog_if=80` | `8086:a0c8` | 500 Series Chipset Family On-Package High Definition Audio | **undriven** — there is no audio on this machine at all, item 3 below |
| `00:1f.4` | `0c05` SMBus | `8086:a0a3` | 500 Series … System Management Bus | **undriven** |
| `00:1f.5` | `0c80` Serial bus controller | `8086:a0a4` | 500 Series … SPI (flash) Controller | **undriven** |
| `00:1f.6` | `0200` Ethernet controller | `8086:15fc` | Ethernet Connection (13) I219-V | **undriven** — gate N's target |
| `04:00.0` | `0108` NVM controller, `prog_if=02` NVM Express | `1c5c:1327` | SK hynix BC501 NVMe Solid State Drive | **driven**, disk refused (below) |
| `09:00.0` | `0280` Network controller (other) | `8086:2725` | Wi-Fi 6E (802.11ax) AX210/AX1675* 2x2 [Typhoon Peak] | **undriven** |
| `0a:00.0` | `0805` SD Host controller, `prog_if=01` | `17a0:9750` | Genesys Logic GL9750 SD Host Controller | **undriven** |

Notes a driver author will want:

- **`8086:15fc` is the `-V` part, not the `-LM`.** `pci.ids` lists `15fb` as
  "Ethernet Connection (13) I219-LM" and `15fc` as "…I219-V"; this machine reports
  `15fc`. The distinction is real (vPro/AMT versus not) and the log settles it.
- **Two xHCI controllers, and the one that matters is the second.** `pci.rs`'s
  enumerate-then-select design exists for exactly this: both match class `0c03`
  prog_if `30`, and a first-match helper would have driven the empty Thunderbolt
  one and missed every internal device.
- **Every driver's selection predicate**, so "undriven" is checkable rather than
  asserted: NVMe takes `matches_class(0x01, 0x08, None)` (first match); xHCI takes
  *all* of `matches_class(0x0C, 0x03, Some(0x30))`; virtio-net/console/sound/gpu
  each take `is_id(0x1AF4, …)`, which nothing on this machine can satisfy. There
  is no other PCI-binding code in the kernel, so the "undriven" column is the
  complement of those four predicates.

### What is not on the PCI bus, and why it blocks M5

**No function in the `00:15.x` range appears.** Tiger Lake's LPSS I2C controllers
are `8086:a0e8`–`a0eb` (I2C #0–#3) and `8086:a0c5`/`a0c6` (I2C #4/#5) per
`pci.ids`; none of those IDs, and no function at device `0x15`, is in the
enumeration.

**One qualification, and it is a finding of its own.** `pci::enumerate` probes
function 0 of each device and `continue`s to the next device when its vendor ID
reads `0xFFFF`, then takes eight functions only if function 0's header type sets
the multi-function bit. So what the log establishes is "**function 0** of device
`0x15` did not answer", not "device `0x15` is empty": a device whose function 0 is
hidden while a higher function still decodes is invisible to this walk, and Intel's
hidden-device mode is exactly the mechanism that could produce that shape. Nothing
here says that is what happened — it is the reason the absence cannot be read as
final without a direct probe of `00:15.1`–`00:15.3`. The same blind spot applies to
every device number on every bus.

`specs/plans/metal-boot-plan.md` M5 plans a native I2C-HID touchpad as "LPSS I2C driver,
ACPI GpioInt, HID multitouch". **The first of those three cannot start from PCI
enumeration on this machine as its firmware is configured.** The likely reason is
Intel SerialIO "ACPI mode", in which firmware hides the PCI function and describes
the controller as an ACPI device with its own MMIO resources — that is a
well-known firmware behaviour, and it is *inference*: this log proves the absence,
not the cause. Either way the consequence is concrete and it is a scope change for
M5: the controller's base address has to come from the ACPI namespace (a DSDT
parser and an AML interpreter, neither of which exists in this kernel), or be
hardcoded from a known-good source and verified, before any I2C transaction is
possible. Nothing else in the tree reads the DSDT — `acpi.rs` handles MADT, MCFG,
HPET and FADT, all fixed-format tables.

The same absence covers the LPSS UART and SPI controllers, and there is no
Integrated Sensor Hub function either.

## USB topology

Both controllers reported identically on capabilities: `max_slots=64`,
`ctx_size=32` (32-byte contexts, HCCPARAMS1 CSZ clear), `pagesize=0x1` (4 KiB
only), and a DMA pool of `2048 KiB: scratchpad=34 device blocks=64 of 12288 B`.

- **34 scratchpad buffers each.** This is the first hardware confirmation of what
  `specs/assessments/metal-track-history.md` argued from the spec: QEMU demands zero
  scratchpad, Intel's controllers demand a real number, and the bug class that
  fixed (a single unaligned buffer written however many were asked for) was
  invisible on the dev host. 34 buffers at 4 KiB is 136 KiB per controller.
- **The USB Legacy Support capability exists and the walk found it.** Boot A:
  `firmware did not claim the controller (USBLEGSUP 0x01002201)` on both. Decoded:
  capability ID `0x01`, next pointer `0x22` dwords, **bit 16 (BIOS-owned) clear**,
  **bit 24 (OS-owned) already set** — firmware had released it before the kernel
  asked. `xhci/legacy.rs`'s own module comment says "QEMU cannot exercise any of
  this… on the only machine in reach the handoff is a walk that finds nothing";
  that is no longer true, and the walk's *finding* half now has a hardware sample.
  Its *waiting* half still does not — nothing has ever made `HANDOFF_TIMEOUT_NS`
  matter.
- **SMI generation: `USBLEGCTLSTS 0xe0000000 -> 0x00000000`.** The five SMI enable
  bits were already clear; the three write-1-to-clear status bits (29/30/31) were
  latched and the write cleared them. So firmware was not arming SMIs on these
  controllers at handoff time. That is the fact behind the argument, recorded in
  `specs/issues/hardware/`, that the port-0x60/0x64 SMM trap is disarmed before
  `i8042::init` — still argued rather than observed, but the premise now has a
  reading.
- **Both controllers were given MSI vector `0x21`.** One vector, two controllers.

### `00:0d.0` — Thunderbolt 4 USB controller

`max_ports=5`, `5/5 root-hub ports powered (PPC=0)`, and the scan found no
connected port at all: the summary line `no HID devices on the controller at
00:0d.0` is printed after `scan_ports` returns, with no port lines before it. It
cost 123 ms (1.065 → 1.188) to establish that. Nothing was plugged into the
Thunderbolt/USB-C ports during either boot, so **this controller has never been
exercised with a device on it**.

### `00:14.0` — PCH xHCI, everything internal

`max_ports=16`, `16/16 root-hub ports powered (PPC=0)`. Five ports connected.

| Port | Speed | Class | ID | Identification | Slot | ToyOS |
|---|---|---|---|---|---|---|
| 3 | 1 | `0xff` | `06cb:00bd` | Synaptics Prometheus MIS Touch Fingerprint Reader | 1 | skipped — `no HID boot interface found` |
| 4 | 3 | `0xef` | `13d3:5406` | IMC Networks (vendor verified); product **not in `usb.ids`** — the integrated camera, from a crowd-sourced probe database, see below | 2 | skipped — `no HID boot interface found` |
| 6 | 3 | `0x00` | `0781:5581` | SanDisk Corp. "Ultra" — the boot stick | 3 | **bound**: `mass storage iface=0 in=0x81/512 out=0x2/512` |
| 9 | 1 | `0x00` | `058f:9540` | Alcor Micro AU9540 Smartcard Reader | 4 | skipped — `no HID boot interface found` |
| 10 | 1 | `0xe0` | `8087:0032` | Intel AX210 Bluetooth | 5 | skipped — `no HID boot interface found` |

USB names are from `usb.ids` (`http://www.linux-usb.org/usb.ids`, fetched
2026-08-03) except `13d3:5406`: the vendor (IMC Networks) is in `usb.ids` and the
product is not. `linux-hardware.org`'s probe database lists `13d3:5406` as "IMC
Networks Integrated Camera", which together with class `0xef` (miscellaneous /
interface-association, the shape of a UVC composite) and a fixed internal port
makes the integrated camera the strong reading — **but it is crowd-sourced, not
canonical, and this file does not assert it as established.**

- **`speed=` is the raw PORTSC Speed field**, bits 10–13. Under the xHCI default
  Protocol Speed ID assignment that is 1 = Full-speed, 2 = Low-speed, 3 =
  High-speed, 4 = SuperSpeed; the driver never reads the Supported Protocol
  capability's PSI table, so a controller that overrode the default mapping would
  be misread. Here the devices corroborate the default: `ep0_packet_from_descriptor`
  accepts only 8/16/32/64 at speed 1 and only 64 at speed 3, and every device's
  stated `bMaxPacketSize0` was accepted.
- **EP0 sizes.** Ports 3 and 9 stated 8 and needed no correction; port 10 stated
  64 and got `port 10 EP0 packet size 8 -> 64`, the Evaluate Context path. Ports 4
  and 6 are high-speed, where 64 is the only legal value and the driver's initial
  guess is already 64. So the full-speed correction path — the one that exists
  because a wrong EP0 size makes every later descriptor read fail — has run on
  real hardware exactly once per boot, on the Bluetooth radio.
- **The boot stick enumerated at High-Speed, not SuperSpeed.** A SanDisk "Ultra"
  is a USB 3.0 part. Whether that is the port it was in, a link that did not train,
  or firmware having left the SS port disabled is **not answerable from this log**.
  The consequence is: every byte the kernel reads or writes on the boot stick —
  the `/log` sink included — moves at USB 2.0 rates on this machine.
- **`no HID boot interface found, skipping` is the correct outcome for four of
  five**, not a failure. None of them offers a boot-protocol HID interface. The
  laptop's keyboard is not here at all; it is PS/2.

## Storage

### NVMe — driven, and the disk is deliberately refused

```
NVMe: found at PCI 04:00.0
NVMe: BAR0=0xbce00000
NVMe: controller enabled
NVMe: NS1 size=500118192 sectors, sector_size=512
NVMe: block device id=1 blocks=62514774 (244198MB)
gpt: device 1 has 4 partitions and none of them is ours
storage: no ToyOS volume and no designation stamp at block 0 — this disk is not ours and nothing will be written to it
```

500118192 × 512 B = 256,060,514,304 B; the kernel's own 4 KiB blocks are
62514774. The disk carries four partitions (a factory Windows layout, unidentified
by this kernel — it parses the GPT and looks only for its own GUIDs) and ToyOS
writes nothing to it. `/home` is therefore a tmpfs on this machine and does not
survive a reboot. That is policy working as designed, and it is the reason no
metal boot has yet exercised the `bcachefs` write path.

### The boot stick — USB mass storage, and a durability finding

Boot A, verbatim (the GUIDs are minted per image build, so boot B's differ):

```
usb-storage: slot 3 vendor "SanDisk." product "Ultra..........."
usb-storage: disk 0 ready on slot 3, 7507812 blocks of 512 B (29327 MiB), msc_block +0x30000
gpt: device 16 carries the log partition B476AE4D-FC70-4B0E-BFA5-CF2E4020F6A3 at LBA 71680+69632, entry 1 of 2
gpt: device 16 carries the boot partition at LBA 2048+69632 (512-byte blocks), entry 0 of 2 on disk 1054565D-B864-46ED-932F-175435FAF62E
boot-volume: partition mounted, 35651584 bytes of a 35651584-byte partition at device offset 1048576, 512-byte sectors, 512-byte clusters, 68552 clusters
log-volume: partition mounted, 35651584 bytes of a 35651584-byte partition at device offset 36700160, 512-byte sectors, 512-byte clusters, 68552 clusters
```

DeviceId 1 is NVMe and USB disks start at 16 (`USB_DEVICE_ID_BASE`), so "device
16" is the stick. Both partitions are 69632 × 512 B = 35,651,584 B, at LBA 2048
and 71680 — device offsets 1,048,576 and 36,700,160.

**Read the size line carefully — it mixes units.** `7507812 blocks` is in the
kernel's 4 KiB host blocks while `512 B` is the *device's* logical sector size,
and the MiB figure is computed from the host block size. 7507812 × 4096 =
30,751,997,952 B = 29327 MiB, a 32 GB stick. Multiplying the two numbers as the
sentence reads gives 3.84 GB, off by 8×. Recorded, not fixed — but anyone quoting
that line should quote the MiB.

**`SYNCHRONIZE CACHE` is refused, and that is an answer rather than a failure:**

```
usb-storage: disk 0 does not implement SYNCHRONIZE CACHE (sense 0x05/0x20/0x00); its writes are durable once they complete
```

Sense key `0x05` / ASC `0x20` / ASCQ `0x00` is ILLEGAL REQUEST / INVALID COMMAND
OPERATION CODE. The driver latches `no_write_cache` and reports success, which is
the right reading: a device with no write cache has nothing the command could have
made durable. What that means for anyone reasoning about the log on this stick:
**there is no flush barrier available on this device.** Durability of
`/log/kernel.log` rests entirely on the write commands having completed, and a
power cut between a write completing and the FAT metadata being written is not
something a cache flush can help with here. The same is true of `/boot`.

Note the timestamp: the refusal is logged at 4.007 s (boot A), *after*
`Boot: complete`, because the first flush is the log sink's own.

## Input — the i8042, and what the machine actually does

The full sequence, boot A:

```
i8042: FADT rev 6 iapc_boot_arch=0x0011, bit 1 (8042) clear — probing either way
i8042: ok selftest=0x55 cfg=0x77->0x64 port1=ok port2=ok
i8042: kbd will not report its scancode set (0xF0 0x00 answered 0xee); firmware's own cfg 0x77 has translate on, so the wire is set 1
i8042: kbd set2+xlat (assumed, the set query was refused) scanning on, GSI 1 -> vec 0x24 apic 0 on
i8042: aux rate=100 res=8/mm, GSI 12 -> vec 0x24 apic 0
i8042: armed at 1941ms, idle at 3883ms, 0 interrupts — the pin has never asserted (kbd GSI 1, aux GSI 12)
i8042: the pin asserts — 1 interrupts, 1 bytes, 1 keys, 0 motion, first seen at 8074ms
```

Boot B is identical but for timing (`armed at 2021ms`, first assert at 12925 ms).

- **The FADT denies its own 8042 and is wrong.** `iapc_boot_arch=0x0011` is
  LEGACY_DEVICES set, **8042 clear**, NO_ASPM set. The controller answers its
  self-test with `0x55` and both interface tests pass. This is settled and closed
  in `specs/issues/hardware/`; it is repeated here because it is a *property of this
  machine* that any future firmware-summary-bit gate will meet again.
- **The keyboard will not report its scancode set.** `0xF0 0x00` is answered
  `0xee` — ECHO's reply, to a command the EC does not implement. The wire format
  is taken from the translate bit firmware left in the config byte (`0x77`), which
  is Linux's entire test on this device class.
- **Both GSIs are identity-mapped, edge, high**, exactly as under QEMU, because
  the ISO table covers neither IRQ 1 nor IRQ 12. Both are routed to vector `0x24`
  on APIC 0. GSI 1 has delivered a real interrupt from a real keypress; **GSI 12
  has still never asserted** in any recorded boot — nothing has touched the
  TrackPoint — so the aux half of the routing is argued from the keyboard half.
- **The byte decodes.** Both boots report `1 bytes, 1 keys`. The open item in
  `specs/issues/hardware/` — "one byte reached the kernel and produced no event" — is
  answered in the direction that matters: on these boots the first byte the EC
  sent produced a key event. That does not retroactively identify the earlier
  `1 bytes, 0 keys` byte (a different keypress, and an extended-key prefix is the
  benign candidate), but the wire format inferred from the translate bit is
  producing decodable keys on this EC.
- **The keyboard drives a shell.** Boot B's session (appendix of `/log/kernel.log`,
  not reproduced here) ran `ls`, `ps`, `toybox`, an unknown command, and
  `shutdown`, each spawning and exiting normally — the integrated keyboard, through
  the i8042, through `keyboard.rs`, into `/bin/console` and `/bin/shell`, on the
  laptop's own panel. **That is the M2 milestone demonstrated end to end on
  hardware.** One caveat on reading that log: it is the raw byte stream the console
  copied to its stdout, so in-line editing is not collapsed and the echoed text is
  not literally what the screen showed.
- **The EC is slow but inside budget.** From the controller self-test to the armed
  keyboard is 1.548 → 1.941 s on boot A: 393 ms, against the driver's staged
  budget of 2100 ms total (`specs/plans/metal-boot-plan.md` M2: 250 + 500 + 750 + 600).
  QEMU's controller does the same sequence in microseconds.

## Display

```
GPU: using UEFI GOP
GOP: 1920x1080 stride=1920 format=1 at 0x4000000000 tokens=[SharedToken(1), SharedToken(2)]
mouse: rel scale x=36 y=64 (screen 1920x1080)
```

`format=1` is BGR (`bootloader/src/main.rs`: `PixelFormat::Rgb => 0, Bgr => 1`),
so the panel is BGRX8888 at 1920×1080, 8,294,400 bytes at physical
`0x4000000000`. **`stride == width`** — the divergence the pre-flash gate flagged
as read-verified-only (§3.3, "QEMU's `stride == width`") is *also* stride == width
on this machine, so the padded-stride path remains unexercised everywhere.

The mode is firmware's and cannot be changed after ExitBootServices. The Iris Xe
at `00:02.0` is untouched: no mode setting, no acceleration, no second output, no
brightness control, no panel power management.

## Boot timing on metal, and where the 3.4 seconds go

`Boot: complete (3422ms)` on boot A, `(3500ms)` on boot B. The recorded QEMU
figure for the comparable configuration is `Boot: complete (196ms)`
(`specs/issues/hardware/`, measured on the `metal_sim_compositor` boot) and `(234ms)` for
the diag artifact booted headless on the metal-sim shape. So metal is **~17×**
the QEMU number, and almost all of the difference is one thing.

### The phases, and the gaps between them

| Phase | Reported | Gap to the next log line |
|---|---|---|
| CPU ready | 60 ms | **450 ms** |
| storage ready | 94 ms | **461 ms** |
| peripherals ready | 876 ms | **471 ms** |
| subsystems ready | 84 ms | **465 ms** |
| devices ready | 14 ms | **461 ms** |
| complete (3422 ms total) | — | **461 ms** |

Boot B: 60 / 94 / 956 / 84 / 14, gaps 449 / 461 / 470 / 464 / 461 / 459.

**Each gap is a `boot_checkpoint()` repaint of the 1920×1080 framebuffer, and the
last one measures it directly.** `boot_phase!` logs the line and then calls
`panic_console::boot_checkpoint()`; in `kernel/src/main.rs` the statement after
`boot_phase!("complete", 0)` is `log!("Keyboard layout: …")` with *nothing* in
between, so the 461 ms (A) / 459 ms (B) between those two timestamps is the
checkpoint and only the checkpoint. The other five gaps are 450–471 ms, so
whatever phase-start work precedes their next log line is inside that spread and
not separable at this resolution.

The paint is a full-screen fill (8,294,400 bytes of stores) plus glyph drawing
plus a `wbinvd`, and which of those dominates is **not** measured here — the
module writes back the whole cache hierarchy on purpose, because the scanout is
mapped write-back over a region firmware marks WC or UC.

**Five of the six paints fall inside the reported boot time: ~2.30 s of 3.42 s,
two thirds of the boot.** This is not a diag-image property. `boot_checkpoint` is
called on every boot and only the virtio-gpu path disables it, so the shipping
GOP image pays the same cost. On QEMU's stdvga the entire boot is 196 ms, which
bounds all six paints there at a few milliseconds total.

Nothing here argues the checkpoints should go — they are the only diagnostic a
machine that wedges without panicking produces, and the trade was made
deliberately. What is new is the **price on real hardware**, which was previously
unknown and is large.

### The other real costs

- **85 ms scanning PCI buses that contain nothing.** Enumeration starts at 0.510
  and the last function (`0a:00.0`) is printed at 0.518; `Enumeration complete`
  is at 0.603. The walk covers all 256 buses, so ~7840 probes of absent
  device/function 0 after the last real one cost 85 ms — about 10.8 µs per absent
  probe on this hardware. (Boot B: 0.517 → 0.602, the same 85 ms.) On QEMU the same
  walk is free, which is why nothing has ever noticed. Total enumeration: 93 ms.
- **~470 ms of USB port enumeration.** 123 ms establishing that the Thunderbolt
  controller's five ports are empty, then 346 ms walking the PCH's five devices.
  Five of those 346 ms are `port N connected` → `port N reset` intervals of 55,
  55, 56, 55 and 55 ms — the controller's own root-port reset, consistent with the
  50 ms USB requires, and not the driver's to shorten. (Boot B's port 6 took 136
  ms for the same step, so the figure is not fixed.)
- **393 ms of i8042 EC init** (above).
- **66 ms bringing up seven APs**, ~11 ms each.
- **50 ms of TSC calibration**, by design.

That accounts for the boot: 1128 ms of reported phase work plus 2308 ms of gaps
is 3436 ms against a reported 3422. The 14 ms excess is an overlap, not a
discrepancy — each gap runs to the *next log line*, which is a few milliseconds
into the following phase, and that phase's own timer has already counted them.

## Contradictions and corrections to existing records

Recorded, not fixed, per this task's scope.

1. **`kernel/src/drivers/pci.rs`'s `MAX_DEVICES` comment says "The T14 Gen 2
   presents 30" functions.** Both logs say **24**. The comment's number is not
   from either of these boots. It is not necessarily wrong for all time — a docked
   machine or anything on the Thunderbolt ports adds functions behind the bridges —
   but as an unqualified statement about this machine it does not match what the
   machine says. The bound itself (256) is unaffected either way.
2. **`specs/plans/metal-boot-plan.md`: "No USB mass-storage driver is needed".** True of
   the *initrd*, which the bootloader reads through UEFI before ExitBootServices —
   and no longer true of the machine. `/boot` and `/log` are both mounts on the
   USB stick, reached through the kernel's own USB mass-storage driver, and the
   kernel log on this machine only exists because of it.
3. **`specs/plans/metal-boot-plan.md` M2 quotes `armed at 1460ms` and `1 bytes, 0
   keys`.** Superseded by these boots: `armed at 1941ms` / `2021ms`, and
   `1 bytes, 1 keys` in both. The 1460 ms figure came from an earlier kernel; the
   `0 keys` observation is not contradicted (different keypress) but is no longer
   the latest evidence.
4. **`xhci/legacy.rs`'s module comment: "QEMU cannot exercise any of this…"** —
   still true of QEMU, but the sentence "on the only machine in reach the handoff
   is a walk that finds nothing" is no longer the whole story: the T14 publishes
   the capability and the walk found it at a real offset.
5. **`specs/plans/metal-boot-plan.md` M4: "the T14's four internal devices enumerate".**
   Confirmed exactly — four internal USB devices plus the boot stick, five slots
   on the PCH controller.

## The undriven list, in the order it is worth attacking

Each entry names the silicon, what it would take, and what is in the way. This is
the metal roadmap's raw material; sequencing is the owner's.

1. **Touchpad — I2C-HID (M5, and the milestone is not complete without it).**
   The device is not on the PCI bus (above). Needs: a route to the LPSS I2C
   controller's MMIO base that does not go through PCI config space, then an LPSS
   I2C driver, then ACPI GpioInt for the interrupt, then HID multitouch. The first
   step is a scope change M5 did not plan for. Softened by a field fact the logs
   could not show: on the 2026-08-03 compositor boot the owner reports the
   touchpad *working* — the T14 mirrors it over the i8042 aux port beside the
   TrackPoint, so I2C-HID buys precision multitouch, not first function.
2. **Ethernet `00:1f.6` `8086:15fc` (I219-V).** Gate N's target on real hardware
   and the only NIC in the machine ToyOS could plausibly drive; the Wi-Fi part is
   not. An e1000e-class driver.
3. **Audio `00:1f.3` `8086:a0c8` (HD Audio / SST).** ToyOS's entire audio stack —
   soundd, gate A, the glitch work, `specs/audio-subsystem-spec.md` — sits on
   virtio-sound, and there is no virtio device in this machine. **On metal today
   there is no audio at all**, and none of gate A's guarantees describe this
   hardware. An HDA controller driver (CORB/RIRB, stream descriptors, codec
   enumeration) is the price of the first note.
4. **Graphics `00:02.0` `8086:9a49` (Iris Xe).** The GOP framebuffer works and
   the compositor is unaware of the difference, so this buys mode setting,
   multiple outputs, acceleration and panel power — none of them blocking.
5. **The NVMe disk.** The controller is driven; the *disk* is refused because it
   carries no ToyOS volume and no designation stamp. Nothing to build — a
   decision to make about whether this machine ever gets a persistent `/home`.
6. **Bluetooth `8087:0032` and Wi-Fi `09:00.0` `8086:2725`.** Both need firmware
   loading and large protocol stacks; both are far out.
7. **SD host controller `0a:00.0` (GL9750), Thunderbolt NHIs `00:0d.2`/`.3`,
   SMBus `00:1f.4`, SPI flash `00:1f.5`, CSME/HECI `00:16.0`.** Enumerated, no
   driver, no current need. Listed so nobody has to re-derive that they exist.
8. **The other skipped USB devices** — fingerprint (`06cb:00bd`), camera
   (`13d3:5406`), smartcard (`058f:9540`). Each is a class driver's worth of work
   and none is on any milestone.

Two further gaps, one from the logs and one not. **The Thunderbolt xHCI has never
had a device on it**, so hotplug — which does nothing at all today,
`specs/issues/hardware/hotplug-blocks-a-scheduler-pass.md` — has never been
exercised on this machine either; that one is the logs'.
**There is no ACPI SCI**, so the lid and the power button raise nothing; that one
is `specs/plans/metal-boot-plan.md` M2's accepted limitation, and these logs neither
confirm nor deny it — all they show is `ACPI: PM1a=0x1804 SLP_TYPa=7`, which is
the shutdown path and nothing to do with events.

## Appendix: boot A, verbatim

`--diag-boot`, 2026-08-02, sha256
`1bf614261cdef978fd8e561bdd082155a808e4f636b31620666c11fc0b911654`.

```
[kernel 0.000 boot] panic console: framebuffer at 0x4000000000 is above the boot map, armed after mm::init
[kernel 0.000 boot] serial: 16550 loopback read 0xff (absent or wrong port)
[kernel 0.000 boot] KernelArgs { memory_map_addr: 1672929304, memory_map_size: 2088, kernel_memory_addr: 1621098496, kernel_memory_size: 11419648, kernel_stack_addr: 3031040, kernel_stack_size: 8388608, rsdp_addr: 2495602708, initrd_addr: 1633660952, initrd_size: 2756608, init_program_addr: 1644752536, init_program_len: 25, kernel_elf_addr: 1636421656, kernel_elf_size: 3545024, gop_framebuffer: 274877906944, gop_framebuffer_size: 8294400, gop_width: 1920, gop_height: 1080, gop_stride: 1920, gop_pixel_format: 1, boot_pml4_addr: 1672876032, boot_partition_start_lba: 2048, boot_partition_blocks: 69632, boot_partition_guid: [20, 105, 215, 204, 46, 217, 156, 66, 134, 70, 135, 233, 77, 6, 83, 123], boot_partition_present: 1, log_partition_guid: [77, 174, 118, 180, 112, 252, 14, 75, 191, 165, 207, 46, 64, 32, 246, 163] }
[kernel 0.000 boot] ACPI: MADT cpus=[0, 2, 4, 6, 1, 3, 5, 7]
[kernel 0.000 boot] LAPIC: x2APIC enabled (ID 0)
[kernel 0.000 cpu0] percpu: BSP cpu_id=0 lapic_id=0 smep=on smap=on pcid=on
[kernel 0.000 cpu0] ioapic: id=2 at 0xfec00000 ver=0x20 gsi 0..119 masked 120/120
[kernel 0.000 cpu0] ioapic: iso bus:irq->gsi [0:0->2 edge/high, 0:9->9 level/high]
[kernel 0.000 cpu0] symbols: loaded 4616 kernel symbols
[kernel 0.000 cpu0] ACPI: HPET at 0xfed00000
[kernel 0.050 cpu0] TSC: 2419MHz (period=413328fs, calibrated over 50ms)
[kernel 0.060 cpu0] LAPIC timer: 384032 ticks/10ms
[kernel 0.060 cpu0] Boot: CPU ready (60ms)
[kernel 0.510 cpu0] ACPI: RSDP at 0x94bfe014
[kernel 0.510 cpu0] ACPI: MCFG found at 0x8f428000
[kernel 0.510 cpu0] ACPI: ECAM base address: 0xc0000000
[kernel 0.510 cpu0] PCI: Enumerating devices...
[kernel 0.510 cpu0]   PCI 00:00.0 [0600] vendor=8086 device=9a14 prog_if=00
[kernel 0.510 cpu0]   PCI 00:02.0 [0300] vendor=8086 device=9a49 prog_if=00
[kernel 0.510 cpu0]   PCI 00:04.0 [1180] vendor=8086 device=9a03 prog_if=00
[kernel 0.510 cpu0]   PCI 00:06.0 [0604] vendor=8086 device=9a09 prog_if=00
[kernel 0.510 cpu0]   PCI 00:07.0 [0604] vendor=8086 device=9a25 prog_if=00
[kernel 0.510 cpu0]   PCI 00:07.2 [0604] vendor=8086 device=9a27 prog_if=00
[kernel 0.510 cpu0]   PCI 00:0a.0 [1180] vendor=8086 device=9a0d prog_if=00
[kernel 0.510 cpu0]   PCI 00:0d.0 [0c03] vendor=8086 device=9a13 prog_if=30
[kernel 0.510 cpu0]   PCI 00:0d.2 [0c03] vendor=8086 device=9a1b prog_if=40
[kernel 0.510 cpu0]   PCI 00:0d.3 [0c03] vendor=8086 device=9a1d prog_if=40
[kernel 0.510 cpu0]   PCI 00:14.0 [0c03] vendor=8086 device=a0ed prog_if=30
[kernel 0.510 cpu0]   PCI 00:14.2 [0500] vendor=8086 device=a0ef prog_if=00
[kernel 0.510 cpu0]   PCI 00:16.0 [0780] vendor=8086 device=a0e0 prog_if=00
[kernel 0.511 cpu0]   PCI 00:1c.0 [0604] vendor=8086 device=a0b8 prog_if=00
[kernel 0.511 cpu0]   PCI 00:1c.4 [0604] vendor=8086 device=a0bc prog_if=00
[kernel 0.511 cpu0]   PCI 00:1c.7 [0604] vendor=8086 device=a0bf prog_if=00
[kernel 0.511 cpu0]   PCI 00:1f.0 [0601] vendor=8086 device=a082 prog_if=00
[kernel 0.511 cpu0]   PCI 00:1f.3 [0403] vendor=8086 device=a0c8 prog_if=80
[kernel 0.511 cpu0]   PCI 00:1f.4 [0c05] vendor=8086 device=a0a3 prog_if=00
[kernel 0.511 cpu0]   PCI 00:1f.5 [0c80] vendor=8086 device=a0a4 prog_if=00
[kernel 0.511 cpu0]   PCI 00:1f.6 [0200] vendor=8086 device=15fc prog_if=00
[kernel 0.512 cpu0]   PCI 04:00.0 [0108] vendor=1c5c device=1327 prog_if=02
[kernel 0.514 cpu0]   PCI 09:00.0 [0280] vendor=8086 device=2725 prog_if=00
[kernel 0.518 cpu0]   PCI 0a:00.0 [0805] vendor=17a0 device=9750 prog_if=01
[kernel 0.603 cpu0] PCI: Enumeration complete, 24 functions.
[kernel 0.603 cpu0] file cache: budget 64144 pages (250 MiB)
[kernel 0.603 cpu0] gpt: the boot volume names B476AE4D-FC70-4B0E-BFA5-CF2E4020F6A3 as the log partition
[kernel 0.603 cpu0] gpt: firmware booted us from partition CCD76914-D92E-429C-8646-87E94D06537B at LBA 2048+69632
[kernel 0.603 cpu0] NVMe: found at PCI 04:00.0
[kernel 0.603 cpu0] NVMe: BAR0=0xbce00000
[kernel 0.603 cpu0] NVMe: controller enabled
[kernel 0.603 cpu0] NVMe: NS1 size=500118192 sectors, sector_size=512
[kernel 0.603 cpu0] NVMe: block device id=1 blocks=62514774 (244198MB)
[kernel 0.604 cpu0] gpt: device 1 has 4 partitions and none of them is ours
[kernel 0.604 cpu0] page cache: 62514774 device blocks, index sized for 7168 cached blocks, cap 4096 slots
[kernel 0.604 cpu0] storage: no ToyOS volume and no designation stamp at block 0 — this disk is not ours and nothing will be written to it
[kernel 0.604 cpu0] Boot: storage ready (94ms)
[kernel 1.065 cpu0] xHCI: found at PCI 00:0d.0
[kernel 1.065 cpu0] xHCI: BAR0=0x603dbb0000
[kernel 1.065 cpu0] xHCI: MSI enabled (vector 0x21)
[kernel 1.065 cpu0] xHCI: max_slots=64 max_ports=5 ctx_size=32 pagesize=0x1
[kernel 1.065 cpu0] xHCI: dma 2048 KiB: scratchpad=34 device blocks=64 of 12288 B (max_slots=64)
[kernel 1.065 cpu0] xHCI: firmware did not claim the controller (USBLEGSUP 0x01002201)
[kernel 1.065 cpu0] xHCI: USBLEGCTLSTS 0xe0000000 -> 0x00000000 (SMI generation off)
[kernel 1.065 cpu0] xHCI: controller reset
[kernel 1.065 cpu0] xHCI: 34 scratchpad buffers configured
[kernel 1.065 cpu0] xHCI: controller started
[kernel 1.065 cpu0] xHCI: 5/5 root-hub ports powered (PPC=0)
[kernel 1.065 cpu0] xHCI: found at PCI 00:14.0
[kernel 1.065 cpu0] xHCI: BAR0=0x603dba0000
[kernel 1.065 cpu0] xHCI: MSI enabled (vector 0x21)
[kernel 1.065 cpu0] xHCI: max_slots=64 max_ports=16 ctx_size=32 pagesize=0x1
[kernel 1.065 cpu0] xHCI: dma 2048 KiB: scratchpad=34 device blocks=64 of 12288 B (max_slots=64)
[kernel 1.065 cpu0] xHCI: firmware did not claim the controller (USBLEGSUP 0x01002201)
[kernel 1.065 cpu0] xHCI: USBLEGCTLSTS 0xe0000000 -> 0x00000000 (SMI generation off)
[kernel 1.065 cpu0] xHCI: controller reset
[kernel 1.065 cpu0] xHCI: 34 scratchpad buffers configured
[kernel 1.065 cpu0] xHCI: controller started
[kernel 1.065 cpu0] xHCI: 16/16 root-hub ports powered (PPC=0)
[kernel 1.188 cpu0] xHCI: no HID devices on the controller at 00:0d.0
[kernel 1.188 cpu0] xHCI: port 3 connected
[kernel 1.243 cpu0] xHCI: port 3 reset, speed=1
[kernel 1.243 cpu0] xHCI: slot 1 enabled (dma +0x50000)
[kernel 1.243 cpu0] xHCI: device addressed
[kernel 1.243 cpu0] xHCI: device class=0xff vendor=06cb product=00bd
[kernel 1.243 cpu0] xHCI: no HID boot interface found, skipping
[kernel 1.243 cpu0] xHCI: port 4 connected
[kernel 1.298 cpu0] xHCI: port 4 reset, speed=3
[kernel 1.298 cpu0] xHCI: slot 2 enabled (dma +0x53000)
[kernel 1.298 cpu0] xHCI: device addressed
[kernel 1.299 cpu0] xHCI: device class=0xef vendor=13d3 product=5406
[kernel 1.299 cpu0] xHCI: no HID boot interface found, skipping
[kernel 1.299 cpu0] xHCI: port 6 connected
[kernel 1.355 cpu0] xHCI: port 6 reset, speed=3
[kernel 1.355 cpu0] xHCI: slot 3 enabled (dma +0x56000)
[kernel 1.416 cpu0] xHCI: device addressed
[kernel 1.416 cpu0] xHCI: device class=0x0 vendor=0781 product=5581
[kernel 1.416 cpu0] xHCI: configuration set
[kernel 1.416 cpu0] xHCI: mass storage iface=0 in=0x81/512 out=0x2/512
[kernel 1.417 cpu0] usb-storage: slot 3 vendor "SanDisk." product "Ultra..........."
[kernel 1.417 cpu0] usb-storage: disk 0 ready on slot 3, 7507812 blocks of 512 B (29327 MiB), msc_block +0x30000
[kernel 1.417 cpu0] xHCI: port 9 connected
[kernel 1.472 cpu0] xHCI: port 9 reset, speed=1
[kernel 1.472 cpu0] xHCI: slot 4 enabled (dma +0x59000)
[kernel 1.473 cpu0] xHCI: device addressed
[kernel 1.474 cpu0] xHCI: device class=0x0 vendor=058f product=9540
[kernel 1.479 cpu0] xHCI: no HID boot interface found, skipping
[kernel 1.479 cpu0] xHCI: port 10 connected
[kernel 1.534 cpu0] xHCI: port 10 reset, speed=1
[kernel 1.534 cpu0] xHCI: slot 5 enabled (dma +0x5c000)
[kernel 1.534 cpu0] xHCI: device addressed
[kernel 1.534 cpu0] xHCI: port 10 EP0 packet size 8 -> 64
[kernel 1.534 cpu0] xHCI: device class=0xe0 vendor=8087 product=0032
[kernel 1.534 cpu0] xHCI: no HID boot interface found, skipping
[kernel 1.534 cpu0] xHCI: no HID devices on the controller at 00:14.0
[kernel 1.534 cpu0] xHCI: 2 controller(s), 0 HID device(s)
[kernel 1.534 cpu0] usb-storage: 1 device(s)
[kernel 1.547 cpu0] gpt: device 16 carries the log partition B476AE4D-FC70-4B0E-BFA5-CF2E4020F6A3 at LBA 71680+69632, entry 1 of 2
[kernel 1.547 cpu0] gpt: device 16 carries the boot partition at LBA 2048+69632 (512-byte blocks), entry 0 of 2 on disk 1054565D-B864-46ED-932F-175435FAF62E
[kernel 1.547 cpu0] i8042: FADT rev 6 iapc_boot_arch=0x0011, bit 1 (8042) clear — probing either way
[kernel 1.548 cpu0] i8042: ok selftest=0x55 cfg=0x77->0x64 port1=ok port2=ok
[kernel 1.548 cpu0] i8042: kbd will not report its scancode set (0xF0 0x00 answered 0xee); firmware's own cfg 0x77 has translate on, so the wire is set 1
[kernel 1.941 cpu0] i8042: kbd set2+xlat (assumed, the set query was refused) scanning on, GSI 1 -> vec 0x24 apic 0 on
[kernel 1.941 cpu0] i8042: aux rate=100 res=8/mm, GSI 12 -> vec 0x24 apic 0
[kernel 1.941 cpu0] ACPI: PM1a=0x1804 SLP_TYPa=7
[kernel 1.941 cpu0] Boot: peripherals ready (876ms)
[kernel 2.412 cpu0] SMP: AP cpu1 lapic=2 online
[kernel 2.423 cpu0] SMP: AP cpu2 lapic=4 online
[kernel 2.434 cpu0] SMP: AP cpu3 lapic=6 online
[kernel 2.445 cpu0] SMP: AP cpu4 lapic=1 online
[kernel 2.456 cpu0] SMP: AP cpu5 lapic=3 online
[kernel 2.467 cpu0] SMP: AP cpu6 lapic=5 online
[kernel 2.478 cpu0] SMP: AP cpu7 lapic=7 online
[kernel 2.478 cpu0] storage: /home is a tmpfs — it will not survive a reboot
[kernel 2.478 cpu0] boot-volume: partition mounted, 35651584 bytes of a 35651584-byte partition at device offset 1048576, 512-byte sectors, 512-byte clusters, 68552 clusters
[kernel 2.479 cpu0] log-volume: partition mounted, 35651584 bytes of a 35651584-byte partition at device offset 36700160, 512-byte sectors, 512-byte clusters, 68552 clusters
[kernel 2.485 cpu0] log-file: this boot's kernel log continues in /log/kernel.log, which holds 0 bytes
[kernel 2.485 cpu0] Boot: subsystems ready (84ms)
[kernel 2.950 cpu0] virtio-console: no device found
[kernel 2.953 cpu0] VirtIO net: no device found
[kernel 2.961 cpu0] GPU: using UEFI GOP
[kernel 2.961 cpu0] GOP: 1920x1080 stride=1920 format=1 at 0x4000000000 tokens=[SharedToken(1), SharedToken(2)]
[kernel 2.961 cpu0] mouse: rel scale x=36 y=64 (screen 1920x1080)
[kernel 2.961 cpu0] Boot: devices ready (14ms)
[kernel 3.422 cpu0] ELF: 2960 relocations indexed (RELATIVE + GLOB_DAT + TPOFF)
[kernel 3.422 cpu0] spawn: TLS 1 modules, total_memsz=144
[kernel 3.422 cpu0] spawn: /bin/toybox pid=0 tid=0 base=0x10000000000 entry=0x100000397a0 cr3=0x143c000 (layout=0ms relocs=0ms deps=0ms tls=0ms total=1ms)
[kernel 3.422 cpu0] spawned /bin/toybox pid=0
[kernel 3.422 cpu0] Boot: complete (3422ms)
[kernel 3.883 cpu0] Keyboard layout: us
[kernel 3.883 cpu4] CPU 4: joining scheduler
[kernel 3.883 cpu2] CPU 2: joining scheduler
[kernel 3.883 cpu1] CPU 1: joining scheduler
[kernel 3.883 cpu5] CPU 5: joining scheduler
[kernel 3.883 cpu3] CPU 3: joining scheduler
[kernel 3.883 cpu7] CPU 7: joining scheduler
[kernel 3.883 cpu6] CPU 6: joining scheduler
[kernel 3.883 cpu2] i8042: armed at 1941ms, idle at 3883ms, 0 interrupts — the pin has never asserted (kbd GSI 1, aux GSI 12)
[kernel 4.007 cpu4] usb-storage: disk 0 does not implement SYNCHRONIZE CACHE (sense 0x05/0x20/0x00); its writes are durable once they complete
[kernel 4.007 cpu0 tid=0] syscalls: pid=0 total=6 syscall_wall=120ms 6=1 9=1 63=1
[kernel 4.007 cpu0 tid=0] memory: pid=0 peak=2MB allocs=2 frees=0
[kernel 4.018 cpu0 tid=0] exit: toybox pid=0 code=0 cpu=124ms
[kernel 8.074 cpu0] i8042: the pin asserts — 1 interrupts, 1 bytes, 1 keys, 0 motion, first seen at 8074ms
```
