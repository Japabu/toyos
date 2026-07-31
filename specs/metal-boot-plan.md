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

- **Display is already fine, and as of `06ce633` that is measured rather than
  assumed.** `kernel/src/main.rs` tries virtio-gpu, then falls back to the UEFI
  GOP framebuffer, both behind one `Gpu` trait; the compositor is unaware. GOP
  cannot change resolution after boot services exit. The GOP branch had never
  executed in this tree before `--gop` existed; on its first run it worked
  end to end — firmware framebuffer mapped, both shared tokens on the same
  scanout, compositor drawing straight into it.
- **The integrated keyboard is PS/2 via i8042** (ThinkPad-standard), not USB —
  the xHCI HID stack is irrelevant to it and ToyOS has no i8042 driver. QEMU q35
  emulates i8042 by default, so the driver develops at full dev speed. The
  TrackPoint is PS/2 on the same controller's aux port.
- **The 16550 probe now has a negative sample.** M0's `UART_PRESENT` latch had
  only ever seen a UART answer; under `--metal-sim` (`-serial none`) the
  loopback reads `0xff` and the kernel boots to the compositor with every UART
  access gated off. The claim "the kernel survives a machine with no 16550" is
  measured rather than argued as of M1.
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

- **M0 — on-screen panic/log console. BUILT** (`2e52e8e`..`883a84d`). The
  kernel survives a machine with no 16550 (the loopback `assert!` that used to
  fire ~20 instructions into `kernel_main` is a probe now, and every UART
  access is gated on it). Fatal panics paint the tail of the log ring as an
  8x16 text grid on the GOP framebuffer, armed *before* `serial::init` and
  taking no lock of any kind; recovering panics never paint. The six boot
  phase boundaries repaint, so a machine that wedges without panicking still
  shows which phase it reached. Six `screen_*` tests decode the screendump
  glyph-by-glyph against the same `font8x16.bin` the kernel blits, and assert
  the fill and highlight colours the decoder is deliberately blind to. What
  the screen carries is the *report*, not the boot log: the ring is drained
  continuously, so only what the panic handler captured before the drain is
  there (known issues §2). Detail:
  `kernel/src/drivers/panic_console/mod.rs`.
- **M1 — "metal-sim" QEMU profile. BUILT.** `cargo run -- --metal-sim` and
  `BootOptions { profile: Profile::Metal }`: firmware GOP, NVMe, xHCI with the
  boot stick on it, q35's i8042, and no virtio device, no USB HID and no 16550
  anywhere. It reaches the compositor, and `metal_sim_compositor` in the suite
  certifies that on every `cargo test` (~4 s) by decoding a QMP screendump —
  the compositor's own `TASKBAR_COLOR` across the full bottom pixel row, a
  composited wallpaper above it, and the kernel's last checkpoint gone from the
  screen. Its teeth: the profile's argv is asserted to contain no `virtio`, no
  USB HID and `-serial none`, because no screendump can see a device that is
  present but unused.

  **The profile keeps no serial, deliberately.** The T14 has no UART, and a
  profile that talks over one is not simulating it — so the guest's only
  channel out is the framebuffer, which is exactly what makes M0 the
  prerequisite it was. `--metal-sim --uart` puts a 16550 back on stdio for
  debugging; anything it shows has to be re-shown without it. The consequence
  for the harness is absolute: `===TEST_START===` cannot survive, because there
  is nothing to send `run <name>` over. A metal-sim guest is observed by
  screendump and by nothing else.

  What it found: `xhci::init` returned `None` when the controller had no HID on
  it and `kernel_main` panicked on that — which is the T14's ordinary state,
  since its keyboard is PS/2 and its touchpad I2C-HID. soundd, netd and sshd
  each panicked on their absent device; they now print one line and exit 0.
  Three residuals are filed in known issues §8: a running system on a
  serial-less machine has no output channel at all, keyboard/mouse claims
  succeed with no hardware behind them, and every network client burns a second
  of retry before giving up. The 2048x2048 mode policy was left alone.

  Still missing from the *simulation*: input. q35 gives the guest an i8042 and
  ToyOS has no driver for it, so metal-sim has no keyboard and no mouse at all.
  That is M2, and it is the last thing between here and the flash trigger.
- **M2 — i8042 driver.** Integrated keyboard + TrackPoint (aux port),
  developed against QEMU's PS/2 emulation. May also yield basic touchpad
  motion on metal if the EC has a PS/2 fallback (unverified).
- **M3 — USB image diet.** `hosted-rustc = false` (the initrd is 666 MB,
  rustc 478 MB of it; see `specs/boot-image-split.md`).
- **M4 — real-firmware robustness.** Fragmented UEFI memory map vs the
  2MiB-only PMM, real ACPI tables, PCI bridges, TSC calibration. xHCI must at
  minimum fail gracefully when internal camera/fingerprint/Bluetooth exhaust
  its 3 hardcoded slots (`kernel/src/drivers/xhci/mod.rs:136-145`); dynamic
  slots are their own known issue. M1 removed the adjacent panic — zero HID
  devices no longer kills the boot — but three is still three, and the T14's
  internal USB devices are what will find that out. M1 also made a missing
  xHCI controller survivable, which nothing has yet had a chance to exercise.
- **FLASH TRIGGER: metal-sim boots to the compositor with the PS/2 keyboard
  working and panics render on screen → flash the stick.** At that point first
  metal boot is an afternoon with readable failures, not a black-screen slog.
- **M5 — native I2C-HID touchpad** (on metal, post-first-boot): LPSS I2C
  driver, ACPI GpioInt, HID multitouch. **The milestone is not complete until
  the real touchpad works** — a PS/2 fallback with no multitouch does not
  count.

A real cyclictest-equivalent for ToyOS should exist before the first metal
boot — it is the instrument that turns the boot into a measurement.
