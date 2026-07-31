# First boot on real hardware — ThinkPad T14 Gen 2

The target machine: Lenovo ThinkPad T14 Gen 2 — Intel i5-1135G7 (Tiger Lake),
16 GB RAM, 256 GB NVMe. The milestone: ToyOS flashed to a USB stick, booting to
the compositor on the laptop's own screen, with the **integrated keyboard and
touchpad** working. No dongles — the built-in input is the milestone.

Position in the roadmap: starts after the soundd idle redesign lands. Not to be
pulled ahead of it. Beyond the milestone itself, the T14 is the only honest
instrument for the ≤2× performance bar: on the dev host, `qemu-system-x86_64`
runs TCG-only (Hypervisor.framework virtualizes ARM64 guests only) and TCG's
distortion is non-uniform — measured 6.5× on address-space switch vs 1.06× on an
ALU loop — so no correction factor exists. Real Tiger Lake silicon scores the
bar; QEMU cannot.

## Starting position (verified 2026-07-30 — re-verify before acting, this ages)

- **Display is already fine.** `kernel/src/main.rs:329` tries virtio-gpu, then
  falls back to the UEFI GOP framebuffer, both behind one `Gpu` trait; the
  compositor is unaware. GOP cannot change resolution after boot services exit.
- **The integrated keyboard is PS/2 via i8042** (ThinkPad-standard), not USB —
  the xHCI HID stack is irrelevant to it and ToyOS has no i8042 driver. QEMU q35
  emulates i8042 by default, so the driver develops at full dev speed. The
  TrackPoint is PS/2 on the same controller's aux port.
- **The touchpad is I2C-HID** behind Intel LPSS I2C, interrupt via Tiger Lake
  GPIO — a real driver stack (LPSS I2C + ACPI GpioInt + HID multitouch) that
  QEMU cannot emulate; it must be debugged on the machine itself. Unverified:
  whether the EC exposes a basic PS/2 fallback for the touchpad. If the T14
  runs Linux, `/proc/bus/input/devices` settles model + interface + fallback
  instantly.
- **No USB mass-storage driver is needed**: the bootloader reads the whole
  initrd via UEFI protocols before ExitBootServices, so firmware handles the
  stick.
- PCID/INVPCID codepaths have never run on real hardware (TCG supports
  neither).

## Stages

- **M0 — on-screen panic/log console.** The T14 has no serial port; without
  this a failed boot is an unreadable black screen. Build first — it also
  improves QEMU debugging.
- **M1 — "metal-sim" QEMU profile.** No virtio devices at all (GOP + i8042 +
  NVMe only); must reach the compositor; daemons must degrade gracefully when
  their device is absent. Becomes a permanent CI config so metal regressions
  are caught at dev speed.
- **M2 — i8042 driver.** Integrated keyboard + TrackPoint (aux port),
  developed against QEMU's PS/2 emulation. May also yield basic touchpad
  motion on metal if the EC has a PS/2 fallback (unverified).
- **M3 — USB image diet.** `hosted-rustc = false` (the initrd is 666 MB,
  rustc 478 MB of it; see `specs/boot-image-split.md`).
- **M4 — real-firmware robustness.** Fragmented UEFI memory map vs the
  2MiB-only PMM, real ACPI tables, PCI bridges, TSC calibration. xHCI must at
  minimum fail gracefully when internal camera/fingerprint/Bluetooth exhaust
  its 3 hardcoded slots (`kernel/src/drivers/xhci/mod.rs:136-145`); dynamic
  slots are their own known issue.
- **FLASH TRIGGER: metal-sim boots to the compositor with the PS/2 keyboard
  working and panics render on screen → flash the stick.** At that point first
  metal boot is an afternoon with readable failures, not a black-screen slog.
- **M5 — native I2C-HID touchpad** (on metal, post-first-boot): LPSS I2C
  driver, ACPI GpioInt, HID multitouch. **The milestone is not complete until
  the real touchpad works** — a PS/2 fallback with no multitouch does not
  count.

A real cyclictest-equivalent for ToyOS should exist before the first metal
boot — it is the instrument that turns the boot into a measurement.
