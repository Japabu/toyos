---
status: open
kind: finding
opened: 2026-08-01
---

# The pre-flash gate certified everything except the milestone

`specs/pre-flash-gate.md` §7 records **GO** at `b82fc4a` with a 182/182 guest
suite. Its six sections are storage safety, image well-formedness, boot-time
panics, the on-screen console, and two sections of "recent changes do not alter
boot". **There is no input section**, and the seventeen-row verdict table has no
input row. Input — the thing M2 exists for and the reason the stick was flashed
— appears only as items 1 and 2 of "What this gate does NOT cover".

That is the hole, and it is not "the gate ran the wrong test". The gate's own
method is to ask a false-pass question per item, and it asks it well for the two
items whose QEMU-versus-hardware divergence it noticed: §3.2 (TCG always reports
FSGSBASE) and §3.3 (QEMU's `stride == width`), both explicitly recorded as
read-verified because QEMU cannot exercise them. The i8042 has **more** such
branches than either, every one of them silent (above), and no item asks about
any of them.

What was actually established, and what was not:

- `metal_sim_input` is a real test and it passes: `cargo test --test toyos-build --
  metal_sim` is 3/3 in 15.7 s, `metal_sim_input` in 9 s. Its guest program
  (`tests/toyos-rust-tests/src/bin/input_events.rs`) prints only bytes it read
  from the two device fds; the assertions are `typed.contains("hello")` and an
  exact `(DX*scale_x, DY*scale_y)` delta with the scale read out of the kernel's
  own boot line; and `metal_sim_argv_check` rules out the classic false pass
  (QEMU routing injected input to a USB HID handler). It certifies i8042
  → userland delivery on QEMU's i8042 and nothing about Lenovo's EC.
- **Its teeth were never re-proved after the rewrite.** `0977c8c` records three
  negative demonstrations (`i8042::init` returning immediately, the aux port
  never enabled, the keyboard GSI never unmasked) — all of them against the
  *pixel* version, which `efbeed7` deleted the same day and replaced with the
  event-parsing version. `efbeed7`'s message proves teeth for
  `screen_late_panic` and not for the new `metal_sim_input`. Nothing suggests it
  is vacuous; it has simply never been shown red.
- **The second artifact, built for the FADT-gate removal.**
  `target/bootable-diag-3f110ad.img`, 35,753,984 bytes, sha256
  `1f3eac841ec343a7f5ad69a9f5964a21d79b2f5e763242ef013bad871eeec3b3`. Built by
  `build::build(.., Boot::Diag)` from a detached worktree at `3f110ad` with a
  clean `git status --ignore-submodules=all`, so none of the five agents'
  uncommitted work is in it; `rust/`, `toyos-ld/target` and `toyos-cc/target`
  symlinked to the main checkout, and a throwaway `src/bin` driver rather than
  `cargo run`, because `toolchain::ensure` re-links the shared rustup toolchain
  from any other root. Its initrd holds exactly one file (`bin/toybox`,
  2,140,152 bytes); the strings `i8042: fault injection armed`,
  `i8042: drain bytes=`, `test-late-panic` and `test-runner` are absent, so it is
  the plain default-feature kernel. Booted headless on the metal-sim shape before
  being handed over: the four `i8042:` lines print, `Boot: complete (234ms)`,
  toybox exits, nothing repaints after.
- The flashed kernel is the tested kernel. `target/bootable-diet.img` contains
  `i8042: kbd set2+xlat` and `i8042: absent (FADT rev ` and does **not** contain
  `i8042: fault injection armed`, `i8042: drain bytes=`, `test-late-panic` or
  `debug-wait`, so it is the plain default-feature kernel that `metal_sim_input`
  boots (`BootOptions::default()` is `kernel_features: &[]`; `src/build.rs:405`
  passes none for a non-debug `--build-only`). The root init string is present
  exactly once and `test-runner` and `librustc_driver` not at all.
- **Two shape dimensions the harness never varies.** Every `BootOptions` defaults
  to `smp: 2` and no input test overrides it; the T14's own boot line reads
  `MADT cpus=[0, 2, 4, 6, 1, 3, 5, 7]`. And all six tests that inject i8042
  input drive a guest that busy-polls `read_nonblock`
  (`i8042_keyboard.rs`, `input_events.rs`); none blocks in `sys_read` or in
  `Poller::wait`, which is what the compositor — the flashed machine's only
  consumer — actually does. The wake path itself is shared with the xHCI HID
  path from `sched/driver.rs:drain_irqs` onward and is exercised by every
  usb-kbd boot, so this is a coverage gap rather than a suspected defect.
- The interrupt topology is the one hardware risk that can be **downgraded**
  rather than assumed, from the T14's own first-boot photograph (`first-boot.jpg`,
  `0e267bb`): `ioapic: id=2 at 0xfec00000 ver=0x20 gsi 0..119 masked 120/120` and
  `ioapic: iso bus:irq->gsi [0:0->2 edge/high, 0:9->9 level/high]`. No override
  covers IRQ 1 or IRQ 12, so `gsi_for_isa_irq` returns identity/edge/high exactly
  as under QEMU; the unit covers both GSIs; and 120/120 masked read-backs prove
  the MMIO window is a real redirection table. `route`'s destination check is
  satisfied by the BSP's `LAPIC: x2APIC enabled (ID 0)`.
