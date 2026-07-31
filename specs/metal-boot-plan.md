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
  shows which phase it reached. The `screen_*` tests decode the screendump
  glyph-by-glyph against the same `font8x16.bin` the kernel blits, and assert
  the fill and highlight colours the decoder is deliberately blind to. They are
  the only tests that read pixels, and deliberately so: the panic console *is*
  the screen, so a screendump is the product there rather than a proxy for it. What
  the screen carries is the *report*, not the boot log: the ring is drained
  continuously, so only what the panic handler captured before the drain is
  there (known issues §2). Detail:
  `kernel/src/drivers/panic_console/mod.rs`.
- **M1 — "metal-sim" QEMU profile. BUILT.** `cargo run -- --metal-sim` and
  `BootOptions { profile: Profile::Metal }`: firmware GOP, NVMe, xHCI with the
  boot stick on it, q35's i8042, and no virtio device and no USB HID anywhere.
  `metal_sim_compositor` certifies it on every `cargo test`: the compositor
  claims the firmware framebuffer and reports the mode it got, soundd and netd
  find no device and exit rather than panic. Its teeth are the argv — no
  `virtio`, no USB HID — because no console line and no screendump can see a
  device that is present but unused.

  **The profile keeps its 16550, deliberately** (reversed 2026-07-31; it was
  mute by default until then). The T14 has no UART, but every defect metal-sim
  has actually found came from the *device shape*, and the absent console found
  exactly one thing — the observability gap now filed in known issues §8. With
  a console the `===TEST_START===` protocol works, so the machine that gets
  flashed is the machine the input tests run on: all five i8042 tests and
  `metal_sim_input` boot this profile. `--metal-sim --mute` takes the 16550
  away again; one test uses it (`screen_panic_muted`), and what it certifies is
  the property that needs a mute machine to mean anything — a kernel panic
  reaching the screen with `uart_present()` false and `panic_flush` draining
  nowhere.

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
- **M2 — i8042 driver. BUILT.** Integrated keyboard + TrackPoint (aux port),
  developed against QEMU's PS/2 emulation. May also yield basic touchpad
  motion on metal if the EC has a PS/2 fallback (unverified).

  It needed an **I/O APIC driver first**: every device vector in this tree is
  MSI-X and the 8259 is masked at `idt::init`, so an ISA pin interrupt had
  nowhere to land. That driver masks every redirection entry firmware left
  behind, which closes a boot-panic hazard that exists on metal today (an
  entry aimed at a vector with no IDT gate becomes #GP). It runs between
  `lidt` and the first `sti`, so exception handlers are live throughout and
  the stray-entry window never opens. Accepted: no ACPI SCI, so no
  power-button or lid events — they were a panic before.

  The wire decoders live in `toyos-ps2/`, a standalone `no_std` crate with 17
  host tests including a 10 M-byte fuzz, on the `toyos-sched/` pattern.

  Four QEMU-side questions the design left open are now answered. `0xF0 0x00`
  reads back **0x41** (keyboard in set 2, controller translating, set-1 on the
  wire), so QEMU honours both `0xF0 0x02` and the XLAT config bit; clearing
  the bit makes it answer 0x02 and the driver refuses to attach rather than
  decode garbage. `-machine q35,i8042=off` **does** clear the FADT
  `IAPC_BOOT_ARCH` 8042 bit, so the gate fires and the ports are never
  touched. IRQ 1 and IRQ 12 are uncovered by q35's override table, so they
  stay identity/edge/high — but that is q35's answer to a per-machine
  question, not a settled fact, and it is the one this list is most likely to
  be wrong about on metal. And the aux port's presence probe, device reset and
  rate/resolution programming all work as specified.

  What QEMU still cannot decide is **R1**: whether the T14's eSPI EC lands in
  set 2 + translation on. The read-back is a full determination of the wire
  format and the driver disables rather than decodes a format it did not ask
  for, so the first metal boot answers it in one short line on the laptop's
  own screen. Contingency is one commit (set-2 tables, clear bit 6).

  Also untested outside QEMU, in rough order of risk: the interrupt topology
  (design §12.5, R3) — QEMU has one textbook I/O APIC at 0xFEC00000 with
  identity GSIs and a five-line override table, while the T14 has
  firmware-programmed RTEs, possibly more than one unit, and a real ISO table,
  so the version-register plausibility gate and `route`'s read-back are what
  make a wrong topology one log line instead of a silently dead keyboard;
  SMM trapping port 0x60;
  real EC timing against the 500 ms/750 ms/600 ms stage budgets, each clamped
  to the 1.5 s total; the mouse framer's 5 ms packet-gap threshold, which is the
  only thing that re-frames a PS/2 pointer stream and assumes both the 100
  samples/s the driver programs and an interrupt latency under ~2 ms — a slower
  ISR splits a packet, which costs one packet and self-heals at the next gap;
  the aux-absent
  path (QEMU always provides one); a keyboard resetting behind our back, which
  is undetectable on this wire because `0xAA` is left Shift's break code under
  translation (filed); and coexistence of a USB and a PS/2 keyboard, which QEMU
  structurally cannot stage — it is argued from one shared held-set and tested
  in-kernel by `input_merge`.
- **M3 — USB image diet.** `hosted-rustc = false` (the initrd is 666 MB,
  rustc 478 MB of it; see `specs/boot-image-split.md`).
- **M4 — real-firmware robustness.** Fragmented UEFI memory map vs the
  2MiB-only PMM, real ACPI tables, PCI bridges, TSC calibration. M1 made a
  missing xHCI controller survivable, which nothing has yet had a chance to
  exercise.

  **The three-slot first-boot blocker is closed.** The driver sizes its DMA
  pool from HCSPARAMS and gives every slot its own block, so the T14's four
  internal devices enumerate; `xhci_many_devices` boots six of them every run
  and `xhci_slot_exhaustion` proves a bus wider than the pool costs the extra
  devices and nothing else. Two xHCI items remain in known issues §8, both
  still M4-shaped: the missing USBLEGSUP ownership handoff before HCRST, which
  QEMU structurally cannot fail and Lenovo firmware can, and hotplug, which
  does nothing at all and became reachable when M1 removed the zero-HID panic.
- **FLASH TRIGGER: metal-sim boots to the compositor with the PS/2 keyboard
  working and panics render on screen → flash the stick. MET.**
  `metal_sim_input` certifies it every run, on the machine shape and the plain
  kernel that get flashed: an in-guest process holds both input fds while the
  host injects, and the assertions are the events it printed — the exact
  relative delta the wire carried (a sign error in dy survives "it moved", and
  PS/2 points the opposite way to the screen), a left button down and up, and
  the typed text. It said nothing about the compositor's reaction from
  2026-07-31 on: the pixel version asserted a click at a fixed taskbar
  coordinate, which made compositor layout part of a kernel-delivery criterion
  and needed thresholds to survive the taskbar's own once-a-second repaint.
  First metal boot is now an afternoon with readable failures, not a
  black-screen slog. M3 and M4 are still worth doing before the flash; the
  trigger condition itself no longer blocks.
- **M5 — native I2C-HID touchpad** (on metal, post-first-boot): LPSS I2C
  driver, ACPI GpioInt, HID multitouch. **The milestone is not complete until
  the real touchpad works** — a PS/2 fallback with no multitouch does not
  count.

A real cyclictest-equivalent for ToyOS should exist before the first metal
boot — it is the instrument that turns the boot into a measurement.
