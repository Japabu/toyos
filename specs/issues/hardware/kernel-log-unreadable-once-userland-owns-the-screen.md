---
status: open
kind: defect
opened: 2026-08-01
---

# A kernel log line still cannot be read on the shipping image once userland claims the framebuffer

The T14 booted from `target/bootable-diet.img` (sha256
`9bda620d…e531aa`, the file still on disk and re-hashed) and reached the
compositor with the integrated keyboard and the TrackPoint dead. The driver's
entire contingency for that — `specs/plans/metal-boot-plan.md` M2, the
pre-flash checklist's "what this gate does NOT cover" item 1, and `1bf5f61`'s
commit message —
is **"one loud line on the laptop's own screen instead of a bisect"**. That line
is not readable, and this is the defect that made the first metal input attempt
uninterpretable.

`panic_console::boot_checkpoint` returns immediately once
`SCREEN_OWNED_BY_USERLAND` is set (`panic_console/mod.rs:478`), and
`device::try_claim(DeviceType::Framebuffer)` sets it as the
compositor's third statement (`compositor/src/main.rs:719`). So the last
kernel screenful ever painted is the one at `Boot: complete`, and the compositor
overwrites it with the desktop a few tens of milliseconds later. Measured on
`cargo test --test toyos-build -- metal_sim --nocapture`, the
`metal_sim_compositor` boot: the three `i8042:` lines at 0.099–0.100 s,
`Boot: complete (196ms)`, and the compositor's own first console line after the
daemon-exit lines at 0.244 s. **The screen carrying the answer is up for well
under a fifth of a second and there is no key that pauses it** — `page_forever`
is reached only from `halt_all_cpus`, so a *successful* boot never pages.

The content is there, which is the frustrating part: 26 kernel log lines
separate the last `i8042:` line from `Boot: complete` in that run, against 67
text rows on a 1920x1080 panel, and the longest line in the range is 158
characters against 240 columns — so the line is on the final boot screen, just
not for long enough to read or photograph by hand.

Consequences, in the order they bite:

- **Every one of the driver's seventeen refusal paths is silent in practice.**
  `i8042::init` has sixteen `return`s that each log one line, plus a success
  line whose tail reads `MASKED` when the unmask failed. On the flashed
  configuration all of them look identical from the owner's chair: a desktop
  with dead input.
- **A keyboard-side refusal also costs the pointer.** Every `return` in the
  keyboard block (`i8042/mod.rs:1015-1075` — `0xF5`, the `0xF0 0x00` read-back's
  five refusing arms, `0xF4`) happens *before* the aux block at `:1077`, so the
  TrackPoint is never initialised either. "Keyboard and TrackPoint both dead"
  therefore discriminates nothing — it is the signature of every failure mode,
  including the ones that are purely keyboard-side. The T14's own first answer
  was one of these, and it is no longer among them: a keyboard that will not
  report its scancode set now attaches on firmware's translate bit and the aux
  block runs, which `i8042_kbd_echo` asserts. The other refusals are unchanged.
- **The intended reading of a dead touchpad is destroyed.** The gate told the
  owner a dead touchpad is expected (I2C-HID, unbuilt) and a keyboard refusal is
  the driver working. Neither statement is checkable without the line.

**Built, as `--diag-boot`.** `diag/system.toml` plus a flag on the build system,
the way `--gop` and `--metal-sim` are flags: it writes `target/bootable-diag.img`
instead of `bootable.img`, so no edit to the shared `system.toml` and no image
left contradicting the committed config. The guarantee is structural rather than
a property of the init list — the compositor is the only process that claims the
framebuffer and it is not built into the image at all — and the kernel and
bootloader binaries are unchanged by the flag, so what the owner reads off a diag
boot is what the shipping kernel does. Gated by `screen_diag_boot`
(`tests/toyos.rs`, in `SCREEN_TESTS`): boots the same config on `Profile::Metal`,
polls until the last checkpoint has painted, holds five seconds, and asserts an
`i8042:` line and `Boot: complete` are still decodable. Teeth: with
`/bin/compositor` put back into the init list the fill check reds on
`[24, 24, 37]` against the checkpoint's `[0, 0, 0]`, and the decoded desktop
carries zero occurrences of either asserted string.

Three things it does **not** give, in the order they will bite:

- **Almost nothing after `Boot: complete` is visible.** The last checkpoint is
  otherwise the last paint on a successful boot, so a daemon that dies later is
  exactly as silent as before. The mode answers "how far did the kernel get and
  what did it say", which is the i8042 question, and nothing else.

  **`--console-boot` is the other half and does not replace this one.**
  `/bin/console` claims the framebuffer, seeds its scrollback from
  `/log/kernel.log` so the boot log survives the claim, and puts a shell
  underneath — so anything after `Boot: complete` is one typed command away.
  What it cannot do is what diag exists for: claiming the screen is exactly
  what stops `boot_checkpoint` painting, so a machine that wedges *before*
  userland shows nothing at all in that mode. Two images, two questions.

  Its own residuals: the seed is read once at startup, because the console
  copies the shell's output to its own stdout and that is the ring `log_file`
  drains — a tail would feed itself; and it needs `/log`, which
  `fat32_adapter::mount` gives only to a machine that booted from USB
  (`specs/issues/boot-media/boot-exists-only-on-a-usb-boot.md`), so on anything
  else the console starts with one line saying the log is not there.

  The one exception is deliberate and is the i8042's own health verdict
  (`d13efa6`). The driver now says once whether the pin it armed has ever
  asserted — a quiet verdict emitted from the first scheduler pass that finds a
  CPU with nothing left to run, and an alive line emitted by the pass the first
  interrupt itself schedules — and repaints the panel through
  `boot_checkpoint` for each, *only* on a machine with no console at all
  (`serial::has_console()`, the same predicate `panic_flush` refuses on). On a
  diag boot that turns the dead-input question into an interaction: the frozen
  screen ends in `armed at 106ms, idle at 221ms, 0 interrupts — the pin has
  never asserted`, the owner presses a key, and either the screen repaints with
  `the pin asserts — N interrupts, N bytes, N keys` or it does not move.
  `screen_i8042_health` is the gate, on a muted metal-sim guest; its teeth are
  a `to_screen` that returns immediately (the line is in the ring and not on the
  glass) and a `verdict_due` that never arms (nothing to paint).

  **It does not reach the shipping image.** `boot_checkpoint` still paints
  nothing once the compositor claims the framebuffer, so on `bootable.img` both
  lines reach the log ring and stop there. The health *signal* is the fix; the
  *surface* is still the open problem this entry is about, and the durable
  answer is a log sink that survives userland — the USB-storage/FAT32/GPT work,
  not another boot mode.
- **The T14 pages, and only the footer says so.** Measured on the shipped image's
  own log: 75 display rows at the panel's 240 columns against a 67-row grid, so
  `pagination` gives two pages and the checkpoint paints `[page 2/2]` with the
  newest 66 rows — the first nine rows of the log are above the window. The first
  `i8042:` line is 19 rows above the end, so it is on that page with room to
  spare. QEMU's stdvga grid is 96x256 and the same log is 74 rows there, i.e. one
  page and no footer, so **the footer branch of `screen_diag_boot` has never
  executed**: it is a guard, not a certification, and the machine that will
  exercise it is the laptop.
- **`kernel/src/main.rs:463` asserts a non-empty init list**, so "spawn nothing"
  is not available; a violated assert would paint a panic report instead of a
  boot log. The list is therefore the least a program in this tree can do,
  `/bin/toybox pwd`. It used to be the shipping list's own first entry,
  `locale --load`, which went with the layout syscall — and the shipping list
  now begins with the compositor, which is the one process this image must not
  contain.
- **Every em-dash in a kernel log line is three dots on the panel.** `font8x16`
  holds codepoints 0x20..=0x7E and `draw_glyph` maps everything else to `.`
  (`panic_console/mod.rs:778`), so a 3-byte UTF-8 `—` renders as `...` and costs
  three columns instead of one. Measured on `screen_i8042_health`'s decoded
  screen: `0 interrupts ... the pin has never asserted`. 44 of the kernel's 448
  `log!` sites contain one, and the i8042's diagnostic lines are among the
  densest. Cosmetic on its own; it is not cosmetic against the T14's 240-column
  wrap, which is what decides whether a line is one display row or two, and
  therefore whether it is on the page the checkpoint paints. Cheapest fix is to
  render the three-byte sequence as a single `-`; the honest one is to stop
  putting non-ASCII in `log!`.

`specs/assessments/metal-log-capture.md` is the durable version of the same problem and its
Phase 2 fixed the *panic* half only.
