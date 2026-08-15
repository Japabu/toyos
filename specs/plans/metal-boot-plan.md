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

**What the machine actually contains is `specs/reference/metal-hardware-inventory.md`** —
all 24 PCI functions, both xHCI controllers' USB topology, the NVMe and the boot
stick, the i8042's findings and the boot's own timing, transcribed from the first
two full metal boots and annotated with what ToyOS drives and what it skips. Read
it before planning any driver. It also answers, for M5, where the touchpad's I2C
controller is not.

**The session checklist.** A session walks this table before anything else. An
entry names a measurement, not a topic, and names what closes it.

| # | measurement | closes |
|---|---|---|
| 1 | one boot with `no-ap-control-regs` armed against one without, same image, same session; record the delta | `specs/issues/kernel/ap-control-registers-inherit-init.md` |
| 2 | transcribe `serial: 16550 loopback read 0xNN (…)` from the metal boot, and the boot's own `Boot: complete (Nms)` beside the last metal-sim reading of the same image | `specs/log-architecture-spec.md` §9.6's *"`Drain::Inline`'s cost on a real UART"* — see below, this closes the T14 half only |

**On item 2, because it is the half of an obligation rather than the whole of
it.** §9.6 asks what `Drain::Inline` costs when every boot record is written
synchronously to a 115200-baud port; QEMU answers instantly and cannot price it,
which is why the row came here. **The T14 cannot price it either** — it has no
SuperIO, so the loopback probe reads `0xFF`, `has_console()` is false and the
mode is a branch not taken. What the metal session *can* close is exactly that:
the probe byte is the evidence that the T14 pays nothing, which is the claim
§4.2 makes about this machine and the one a flashed image depends on. The
115200-baud arm needs a machine with a real port and stays open until there is
one; the arithmetic §4.2 states for it (~40 KB at ~87 µs/byte, so seconds) is a
prediction and says so.

## Pre-flash checklist (verified 2026-08-13 — re-verify before acting, this ages)

Run before flashing any image to the T14. It is a **written verdict**, not a
test run: record each result and sign off, or do not flash. Freeze the
filled-in results as a new dated file under `specs/assessments/`, named for
that flash's date — see `specs/assessments/pre-flash-gate-2026-08-01.md` for
the shape of one.

**A green suite is not this gate.** A green suite says the tests pass, not
that the change is right — `specs/assessments/metal-track-history.md` records
a dozen certifications that could not fail. Every item below has a **false
pass** column: the way the check can report success while the property is
broken. If you cannot rule out the false pass, the item is a no-go regardless
of what the terminal printed. `specs/README.md`'s spec-checking method is how
to find more false passes than the ones already listed here.

Run on a **quiet tree** — no other agent building or booting. Contention
produces failures that look like defects and, worse, passes that look like
verdicts.

**Audit the delta before running the items below.** Run
`git log <the commit the last pre-flash assessment covered>..HEAD --name-only`
over `kernel/ bootloader/ src/ toyos-abi/ toyos/ userland/`, and add a
"boot is unchanged" item — run, expect, false pass — for anything that
touched the boot path. List the audited commits by hash, so the next session
knows exactly when this section stopped being true.

### What this gate does NOT cover

State these to the owner before he flashes. They are not gaps to be closed
here; they are things only the hardware can answer.

1. **Whether the T14's EC lands in scancode set 2 with translation on.** QEMU
   cannot decide it. The driver's `0xF0 0x00` read-back determines the wire
   format and refuses to attach to one it did not ask for, so the answer
   appears as one line on the laptop's own screen rather than as a bisect. **A
   refusal to attach is the driver working**, not a regression.
2. **The touchpad is I2C-HID and unbuilt.** A dead touchpad is the expected
   outcome. Do not treat it as a regression, and do not let it consume
   debugging time on the machine.
3. **Real-hardware performance.** TCG cannot measure the 2× bar. The T14 is
   the first honest instrument; nothing here substitutes.
4. **Anything the on-screen console cannot show.** Input is dead on that
   machine and there is no serial, so the console is the entire diagnostic
   channel. If it is broken, every other failure this boot becomes silent —
   which is why §4 is the gate's highest-severity section.

### 1. Storage: nothing may write to a disk it was not given

**The highest-consequence section.** This cannot be caught by the harness:
every scratch image the harness creates *is* designated, so a regression that
widens the write path stays green through the entire suite and only
misbehaves on a disk that is not ours — his. Defeating that property is what
§1.1 and §1.2 exist to do. Do not substitute "the storage tests pass".

#### 1.1 The designation stamp is still the only gate

| | |
|---|---|
| **Run** | Read `bcachefs/src/superblock.rs`. Confirm `DESIGNATION_MAGIC = b"TOYOS-FORMAT-ME\0"` at `:24` and `DESIGNATION_BLOCKS_OFFSET = 16` still exist, and that the `const` assert keeping `DESIGNATION_MAGIC`'s first four bytes distinct from `MAGIC` (`:30-33`) still compiles. Then `git log -p --since=<last flash> -- bcachefs/src/superblock.rs kernel/src/bcachefs_adapter.rs` and read **every** hunk. |
| **Expect** | The stamp is checked before any write path is reachable, and no commit in the range adds a second way to reach `format()`. |
| **False pass** | A new caller reaches a write path *without* consulting the stamp — the stamp is intact and simply no longer the only gate. Reading the constant proves the constant; only reading the callers proves the interlock. Also: a commit that makes the check permissive (warn-and-continue instead of refuse) leaves both the constant and the call site looking correct. |

#### 1.2 `format()` and `mount()` are still private behind `probe`

| | |
|---|---|
| **Run** | `git grep -n "pub fn format\|pub fn mount\|pub fn probe" -- kernel/src/bcachefs_adapter.rs` |
| **Expect** | `fn format()` and `fn mount()` **without** `pub` (currently `:421`, `:436`), and every public route to `format()` passing through `probe()` (currently `:473`). The module has three public entries, not one — `probe`, `open_home` and `mount_initrd` — and the property is that `open_home` reaches `format()` only through `probe`'s `Designated` arm, not that `probe` stands alone. |
| **False pass** | `pub(crate)` reads as "not public" to a skim but opens the whole kernel. Grep for `pub(` too. Equally: `probe` itself gaining a parameter that lets a caller skip the designation check — the signature is public API, so a new argument is a widened gate that this grep will not show. And the grep alone cannot see a *new* public wrapper that reaches `format()`: list the module's `pub` items, do not just match the three names above. |

#### 1.3 No write path bypasses `probe`

| | |
|---|---|
| **Run** | `git grep -n "raw_block_write\|write_block\|NvmeBlockDevice" -- kernel/src` and enumerate every caller. For each, establish it is downstream of `probe`. |
| **Expect** | Every writer is reached only via a `Storage` returned by `probe`. |
| **False pass** | An enumeration that covered only `kernel/src`. Per `specs/plans/fork-lint-audit-plan.md`, **"I enumerated the call sites" is only true if the enumeration covered `~/.cargo/git/checkouts/`.** Check there too. |

#### 1.4 Present the disk read-only for the first boot, if the owner accepts

| | |
|---|---|
| **Run** | Ask the owner whether the boot can run with the internal NVMe physically or firmware-disabled. |
| **Expect** | A decision, recorded. |
| **False pass** | n/a — this is a recommendation, not a check. It is the only item that makes §1's failure survivable rather than merely unlikely, so record his answer either way. |

### 2. The image is flashable

#### 2.1 Sector alignment

| | |
|---|---|
| **Run** | `cargo run -- --build-only`, then `ls -l target/bootable.img` and `python3 -c "import os;n=os.path.getsize('target/bootable.img');print(n, n%512)"` |
| **Expect** | Remainder `0`. `src/image.rs:362`'s `assert_eq!(total_size % 512, 0, ...)` should also still be present. |
| **False pass** | The assert covers the *computed* `total_size`, not the bytes actually written. If a later step appends or truncates, the assert passes and the file is still misaligned — **measure the file on disk, not the constant.** |

#### 2.2 The backup GPT is present at the very end

| | |
|---|---|
| **Run** | Read the last 512 bytes and confirm the GPT backup header signature (`EFI PART`). |
| **Expect** | Signature present in the final sector. |
| **False pass** | A primary GPT at the front makes `fdisk -l` look entirely healthy while the backup is missing. Check the **tail** specifically. |

#### 2.3 It still fits, and boots without the hosted rustc

| | |
|---|---|
| **Run** | Record image size. Then set `hosted-rustc = false` in `system.toml` (currently `:7`), rebuild, and boot the resulting image under a metal-sim-equivalent profile. **Revert `system.toml` afterwards.** |
| **Expect** | Boots to `Boot: complete` without the hosted rustc in the initrd. |
| **False pass** | Testing only the `hosted-rustc = true` image proves nothing about the smaller one, and a stale `target/` can leave the old initrd in place — confirm the image size actually dropped before believing the boot. Nothing in this tree boots the root `system.toml`'s image: every `cargo test` boot builds from `tests/metalcase/system.toml` or `tests/testcases/system.toml`, neither of which carries the key. So this item's boot half needs `cargo run`, which opens a window on the owner's desktop — treat it as inspection plus a size measurement, and record the boot itself as unchecked. |

### 3. Boot-time panics stay closed

For each, the check is the **same shape**: confirm the guard exists *and* that
a test exercises the absent-device path. A guard with no test is a guard
nobody has seen fail.

#### 3.1 NVMe absence

| | |
|---|---|
| **Run** | `cargo test -- diskless` (`Profile::Diskless`, `tests/common/qemu.rs`). |
| **Expect** | Boots to completion with no NVMe controller present. |
| **False pass** | The profile silently still attaching a disk. Confirm from the QEMU command line that no NVMe device is passed, not from the test name. |

#### 3.2 CR4's required features are checked against CPUID, not assumed

| | |
|---|---|
| **Run** | Read `kernel/src/arch/control_regs.rs`. Confirm `CR4_REQUIRED` (`:77`) includes `FSGSBASE`, and `declaration()` (`:159`) computes `missing = CR4_REQUIRED & !supported()` and asserts it is zero before any CPU applies the register. |
| **Expect** | A CPU lacking a required bit is refused by a named assert (`control_regs: cpu{cpu_id} lacks CR4 bits ...`), never left to `#UD` at the first `rdfsbase`/`wrfsbase`. |
| **False pass** | **QEMU's TCG always reports every bit in `CR4_REQUIRED` present**, so the refusal branch never executes in any test on any profile — untestable here by construction. The `control_regs`, `control_regs_verdict` and `control_regs_negative` gates (`tests/toyos.rs`) exercise the declaration and its self-check on the CPUs QEMU does present; record the missing-bit path itself as read-verified only. |

#### 3.3 Framebuffer extent

| | |
|---|---|
| **Run** | Read `kernel/src/drivers/gop.rs` and confirm the extent assert is against `stride × height × 4` (currently `:50`), not `width × height × 4`. |
| **Expect** | The check uses `stride`. |
| **False pass** | **On QEMU, `stride == width` on most modes**, so a regression to `width` is invisible in every emulated boot and only faults on hardware whose stride exceeds its width — which is the common case on real panels. Read the expression; do not infer it from a passing boot. |

#### 3.4 xHCI with no HID device

| | |
|---|---|
| **Run** | `cargo test -- xhci` including the `MetalUsb` profile. Read `kernel/src/drivers/xhci/wait/boot.rs`'s `init` (currently `:154`). |
| **Expect** | `init` returns `()` unconditionally: a controller that enumerates zero HID devices logs `xHCI: no HID devices on the controller at ...` and is kept in the controller list, not panicked on; a machine with no controller at all logs and returns. |
| **False pass** | Every USB profile in the harness attaches at least one HID except `MetalUsb`. Confirm the zero-HID log line actually printed in a `MetalUsb` boot, not just that the test name ran. |

### 4. The on-screen console — the only diagnostic channel

**If this section fails, do not flash.** With no serial and dead input, a
broken console means every other failure is silent and the boot is
uninterpretable.

#### 4.1 It renders on GOP

| | |
|---|---|
| **Run** | `cargo test -- screen` — the suite in `tests/toyos.rs`: `screen_decoder`, `screen_recoverable_untouched`, `screen_early_panic`, `screen_late_panic`, `screen_paged_scrollback`, `screen_panic_muted`, `screen_fatal_halt`. |
| **Expect** | All pass. These decode the framebuffer glyph-by-glyph against `font8x16.bin`; they are the only pixel-reading tests in the tree. |
| **False pass** | **`screen_late_panic` passes with `panic_console::capture`'s body replaced by `return`** — a known dead gate (`specs/issues/`). So a green `screen` suite does **not** establish that capture works. Treat these as covering *rendering*, not capture. |

#### 4.2 Scrollback is retained and pages on a timer

| | |
|---|---|
| **Run** | `cargo test -- screen_paged_scrollback`. Confirm paging is driven by a timer, not a keypress. |
| **Expect** | Multiple pages rendered with no input. |
| **False pass** | A test that supplies a keypress, or one that asserts only the first page. **Input is dead on the T14** — if paging needs a key, the owner sees page one forever and nothing indicates more exists. Confirm the test injects no input at all. |

#### 4.3 A fatal panic reaches the screen with no serial

| | |
|---|---|
| **Run** | `cargo test -- screen_panic_muted` (the `--mute` shape: metal-sim with the 16550 removed — the T14's literal configuration). |
| **Expect** | The panic report renders on the framebuffer with no serial present. |
| **False pass** | Passing under a profile that still has serial. Confirm the muted profile actually removes the UART, since this is the single most important behaviour on the machine. |

### Verdict

Record, per section: **pass / fail / read-verified-only**, and name who
checked.

**Go only if:**

- §1 is pass, with §1.1–§1.3 confirmed by *reading callers*, not by a green
  suite.
- §2 is pass, with alignment and the backup GPT measured **on the file**.
- §4 is pass — no exceptions; it is the only channel that reports anything
  else.
- §3 and the delta audit above are pass or explicitly recorded as
  read-verified with the reason (§3.2 and §3.3 are expected to be
  read-verified; QEMU cannot exercise either).

**No-go on any unresolved false pass**, even where the command printed
success. That is the entire purpose of this checklist.

Two items this gate cannot close on its own, to be recorded rather than
resolved:

- **§2.3's boot half.** `hosted-rustc` lives in the root `system.toml`, which
  governs `target/bootable.img` alone, and nothing in this tree boots that
  image except `cargo run`, which opens a window on the owner's desktop. The
  size drop is measurable; the boot is not, short of the owner's own machine.
- **§1.4 is the owner's decision, not a check.** Record his answer; absence
  of an answer is not a fail.

State the three uncovered items from "What this gate does NOT cover" to the
owner before he boots, so a scancode-set refusal or a dead touchpad is not
mistaken for a regression on a machine where debugging is nearly blind.

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
  there. Detail: `kernel/src/drivers/panic_console/mod.rs`.
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
  exactly one thing — the observability gap now filed as
  `specs/issues/panic-path/no-console-between-boot-and-terminal.md`. With
  a console the `===TEST_START===` protocol works, so the machine that gets
  flashed is the machine the input tests run on: all five i8042 tests and
  `metal_sim_input` boot this profile. `--metal-sim --mute` takes the 16550
  away again; four tests boot it muted, and `screen_panic_muted` certifies
  the property that needs a mute machine to mean anything — a kernel panic
  reaching the screen with `uart_present()` false and `panic_flush` draining
  nowhere.

  What it found: `xhci::init` returned `None` when the controller had no HID on
  it and `kernel_main` panicked on that — which is the T14's ordinary state,
  since its keyboard is PS/2 and its touchpad I2C-HID. soundd, netd and sshd
  each panicked on their absent device; they now print one line and exit 0.
  Three residuals are filed: a running system on a serial-less machine has no
  output channel at all
  (`specs/issues/panic-path/no-console-between-boot-and-terminal.md`),
  keyboard/mouse claims succeed with no hardware behind them
  (`specs/issues/hardware/device-claim-succeeds-with-no-device.md`), and every
  network client burns a second of retry before giving up
  (`hardware/network-clients-pay-a-boot-retry`, since closed). The 2048x2048
  mode policy was left alone.

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
  wire), so QEMU honours the XLAT config bit; clearing
  the bit makes it answer 0x02 and the driver refuses to attach rather than
  decode garbage. `-machine q35,i8042=off` **does** clear the FADT
  `IAPC_BOOT_ARCH` 8042 bit — which was how the gate got tested, and is now how
  its removal is: see **R0** below, since QEMU derives that bit from the
  presence of the device and so cannot make the two disagree.
  IRQ 1 and IRQ 12 are uncovered by q35's override table, so they
  stay identity/edge/high — but that is q35's answer to a per-machine
  question, not a settled fact, and it is the one this list is most likely to
  be wrong about on metal. And the aux port's presence probe, device reset and
  rate/resolution programming all work as specified.

  **R0 — ANSWERED ON THE LAPTOP, and it changed the design.** The first
  `--diag-boot` run printed one line and stopped there:

  ```
  i8042: absent (FADT rev 6 iapc_boot_arch=0x0011)
  ```

  The checksum passed, so this is firmware speaking rather than an unreadable
  table. `0x0011` is `LEGACY_DEVICES` set, **8042 clear**, `NO_ASPM` set
  (ACPICA `actbl.h`; ACPI 6.5 §5.2.9.3). The FADT contradicts itself — legacy
  devices present, no 8042 — and the gate believed the half that was wrong: the
  driver refused on bit 1 and never touched the controller, so the keyboard and
  the TrackPoint were never given a chance to answer.

  **Bit 1 no longer gates the probe.** Not a fallback and not a quirk: the
  driver's own handshake — config-byte read-back, `0xAB` port interface test,
  `0xF0 0x00` verified against `0x41` — is direct observation of the machine,
  and a coarse vendor-written summary bit standing in front of it is backwards.
  The claim is still logged, because the disagreement is the diagnosis. Safety
  on a machine that genuinely has nothing there is the floating bus: `0xff` from
  port 0x64 is every status bit set at once, which no controller produces, so
  the probe refuses in one `inb` rather than waiting out the init budget.
  `i8042_absent` and `i8042_fadt_denial` are the two gates.

  **R1 — ANSWERED ON THE LAPTOP, and it changed the design too.** The question
  was which wire format the T14's eSPI EC lands in. The answer is that it
  will not say. With bit 1 no longer gating, the probe got all the way to the
  keyboard and stopped one step from the end:

  ```
  i8042: ok selftest=0x55 cfg=0x77->0x64 port1=ok port2=ok
  i8042: kbd cmd 0x02 answered Some(238), not ack
  i8042: kbd refused scancode set 2 ... disabled
  ```

  Self-test `0x55`, both interface tests passed, config byte read `0x77` and
  written back `0x64`: the controller is real and healthy, and `0xF5` had
  already been acknowledged, so the keyboard answers commands. 238 is `0xEE`,
  ECHO's own reply, returned for the **argument byte** of `0xF0 0x02` after the
  command byte was acked. The refusal worked as designed and cost the keyboard
  and — because the aux block sits past it — the TrackPoint as well.

  **The driver reads the set now and never writes it.** Nothing else in that
  machine's life issues the write: Linux's `atkbd_select_set` returns set 2
  outright when `atkbd->translated`, which `i8042.c` derives from the XLATE bit
  of the CTR the BIOS left, and `atkbd_skip_getid` withholds even `0xF2` from
  every portable device; EDK2's `Ps2KeyboardDxe` selects a set only under
  `ExtendedVerification`, which its own comment says is skipped when booting an
  OS. A write cannot improve on a read that already answers, and it leaves an EC
  that mishandles it in a state nothing can name.

  **And a refusal of the read is no longer the end.** The read-back stays the
  determination wherever the device gives one. Where it does not, the wire
  format falls back to the translate bit *firmware itself left in the config
  byte* — `before & CFG_TRANSLATE`, which on the T14 is `0x77`. That is not a
  weaker read-back, it is Linux's entire test, on the same byte; enabling a
  set2→set1 translator is coherent only for a device emitting set 2, so
  firmware having enabled it is a statement about the wire made by the one party
  that had a working keyboard on it. Firmware having left translation *off* says
  nothing, and there the driver still refuses. The success line says which of
  the two happened — `(readback 0x41)` or `(assumed, the set query was
  refused)` — so the panel never claims a determination that was not made.

  `i8042_kbd_echo` is the gate: the `i8042-kbd-echo` feature answers the query's
  argument byte with `0xEE` on QEMU's otherwise-perfect keyboard, because
  QEMU implements `0xF0` to the letter and no host-side property turns that off.
  Its teeth are the delivery assertion — the same "hello" the other input tests
  type — since a driver that logs the assumption and arms nothing passes every
  log-line assertion in it.

  **The boot after that one worked.** First time on the metal it was written
  for:

  ```
  i8042: kbd set2+xlat (assumed, the set query was refused) scanning on, GSI 1 -> vec 0x24 apic 0 on
  i8042: aux rate=100 res=8/mm, GSI 12 -> vec 0x24 apic 0
  i8042: armed at 1460ms, idle at 3394ms, 0 interrupts ... the pin has never asserted
  i8042: the pin asserts ... 1 interrupts, 1 bytes, 0 keys, 0 motion, first seen at 11375ms
  ```

  Three things that had never happened. The driver attaches. The **aux port
  initialises fully** — `rate=100 res=8/mm` is the TrackPoint answering its
  whole reset/id/rate/resolution sequence, unreachable before because every
  keyboard-side refusal returns ahead of that block. And a physical keypress
  raised a real interrupt on GSI 1, which retires **R3 for the keyboard line**:
  the topology read off the first-boot photograph is the topology that delivers,
  so `route`'s read-back, the identity GSI and the unmask are all correct on
  Tiger Lake. GSI 12 is programmed the same way and has still never asserted —
  nothing has touched the TrackPoint — so the aux half of R3 is argued from the
  keyboard half, not observed.
  Two measurements fall out: the EC is slow but inside its budget (`armed at
  1460ms` against a 2100 ms total), and `Boot: peripherals ready` went 6 ms →
  398 ms, which is the aux reset stage running against a device that takes real
  time rather than QEMU's microseconds.

  **What it did not do is decode.** `1 bytes, 0 keys` — and the counters could
  not name a suspect, because 84 of the 256 single byte values decode to nothing
  under set 1 and `handle_key` drops a break for a usage nothing held. An
  extended key's `0xE0`, where nothing is wrong, is indistinguishable in that
  arithmetic from `0xAA`, `0xFA`, `0xEE` or a raw set-2 Enter (`0x5A`). The
  health line now names the bytes that produced no event, and revises itself
  once if a later byte does decode — `i8042_undecoded_bytes` gates both, by
  injecting Pause, the one key whose whole sequence is swallowed by design.
  Filed as `specs/issues/hardware/t14-keyboard-will-not-report-its-scancode-set.md`;
  the next diag boot answers it in one line.

  What is left of R1 is the residue: `0xEE` is a *response* byte, and the only
  defined meaning of `0xEE` on this wire is ECHO's reply, so an EC that answers
  it to something nobody echoed is an EC answering a command it does not
  implement. Ruled out as the source: translation mangling the ack (the same
  translator passed two `0xFA`s in the two commands immediately before, and the
  standard table is identity above `0x80` except `0x83`→`0x41` and
  `0x84`→`0x54`); a stale or aux byte (scanning was off by the acked `0xF5` and
  the aux clock off by `0xA7`, so both device ports were silent by the
  controller's own configuration, and no set-2 scancode translates to `0xEE`);
  and timing (the two preceding acks in the same exchange were read correctly).

  Also untested outside QEMU, in rough order of risk: the interrupt topology
  (design §12.5, R3) — QEMU has one textbook I/O APIC at 0xFEC00000 with
  identity GSIs and a five-line override table, while the T14 has
  firmware-programmed RTEs, possibly more than one unit, and a real ISO table,
  so the version-register plausibility gate and `route`'s read-back are what
  make a wrong topology one log line instead of a silently dead keyboard;
  SMM trapping port 0x60 — the xHCI USBLEGSUP handoff runs immediately before
  `i8042::init` and clears the controller's SMI enables, so the USB legacy
  emulation that would trap those ports is disarmed by then;
  real EC timing against the 500 ms/750 ms/600 ms stage budgets, which since
  `d13efa6` sum *into* the total rather than past it (250 + 500 + 750 + 600 =
  2100 ms); the mouse framer's 5 ms packet-gap threshold, which is the
  only thing that re-frames a PS/2 pointer stream and assumes both the 100
  samples/s the driver programs and an interrupt latency under ~2 ms — a slower
  ISR splits a packet, which costs one packet and self-heals at the next gap;
  the aux-absent
  path (QEMU always provides one); a keyboard resetting behind our back, which
  is undetectable on this wire because `0xAA` is left Shift's break code under
  translation (filed); and coexistence of a USB and a PS/2 keyboard, which QEMU
  structurally cannot stage — it is argued from one shared held-set and tested
  in-kernel by `input_merge`.
- **M2.5 — a console on the panel. BUILT.** `cargo run -- --console-boot`
  builds `target/bootable-console.img`: `/bin/console` claims
  `DEVICE_FRAMEBUFFER`, reuses `/bin/terminal`'s emulator (which never knew the
  compositor was below it — `Console::new` always took a raw framebuffer), and
  runs `/bin/shell` on the glass with the bytes the kernel already translates
  off the i8042. It is what turns every further question about the T14 from a
  reflash and a photograph into a typed command, and it is the first thing that
  puts a character on that panel *from that keyboard*.

  It starts with the boot log above the prompt. Claiming the framebuffer sets
  `SCREEN_OWNED_BY_USERLAND`, after which `boot_checkpoint` never paints again,
  so a console that merely cleared the screen would have traded the diagnostic
  that works today for one that might. No syscall reads the kernel's log ring;
  `log_file` writes the same bytes to `/log/kernel.log` and seeds that
  sink from the ring's *retained* window, so the file starts at the boot's first
  line. Measured on the first metal-sim boot: 6768 bytes, 87 kernel log rows on
  the panel above the prompt, `i8042:` lines included.

  A fatal panic still takes the screen back — `render` ignores
  `SCREEN_OWNED_BY_USERLAND` entirely, only `boot_checkpoint` honours it — and
  that is now staged rather than read: `screen_console_panic` triggers the panic
  *through* the console, by typing at its prompt. Nothing had staged it before,
  because `screen_fatal_halt` boots `tests/testcases`, whose init list has no
  framebuffer claimer in it.

  `screen_console_shell` is the gate that matters: it types `echo zqjxk` on the
  emulated i8042 and asserts the *output* is a row of the panel. A
  prompt-only assertion would pass on a console that cannot read the keyboard,
  which is the path this exists to bring up.

  What it does **not** answer, and only the laptop can: whether the T14's EC
  produces bytes that *decode*. The last metal boot logged `1 bytes, 0 keys`,
  and this program is downstream of that — it renders what
  `kernel/src/keyboard.rs` translated, so a wire the driver reads wrongly types
  nonsense here rather than nothing. The health line and
  `i8042_undecoded_bytes` are still what name the byte; this is what makes the
  answer readable without a reflash.
- **M3 — USB image diet.** `hosted-rustc = false` (the initrd is 666 MB,
  rustc 478 MB of it; see `specs/plans/boot-image-split.md`).
- **M4 — real-firmware robustness.** Fragmented UEFI memory map vs the
  2MiB-only PMM, real ACPI tables, PCI bridges, TSC calibration. M1 made a
  missing xHCI controller survivable, which nothing has yet had a chance to
  exercise.

  **The three-slot first-boot blocker is closed.** The driver sizes its DMA
  pool from HCSPARAMS and gives every slot its own block, so the T14's four
  internal devices enumerate; `xhci_many_devices` boots six of them every run
  and checks the block count against the controller's slot count rather than
  against a number. `xhci_slot_exhaustion` proves a bus wider than the pool
  costs the extra devices one log line each, and that the device which did get
  a block was enumerated to completion — but not that a HID survives the
  shortage and delivers, because the one device that fits is the boot stick:
  QEMU puts it on the first SuperSpeed port register, ahead of every USB2 one,
  so it takes slot 1 and binds nothing. One xHCI item remains
  (`specs/issues/hardware/hotplug-blocks-a-scheduler-pass.md`)
  and it is still M4-shaped: hotplug does nothing at all, and became reachable
  when M1 removed the zero-HID panic. The USBLEGSUP ownership handoff is built
  (`xhci/legacy.rs`), runs before the HCRST, and disarms the controller's SMI
  enables — but QEMU publishes no Legacy Support capability, so what a green
  suite certifies is that the walk terminates and runs in the right order, not
  that a handoff ever happened.
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
