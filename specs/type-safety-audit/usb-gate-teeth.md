# The USB mass-storage gates, attacked

Read-only audit of the *tests*, not the driver — the code audit is
`specs/type-safety-audit/usb-storage.md`, by a different agent, and the two
overlap on exactly one item (`healthy()`). Scope: `usb_storage_*`,
`Profile::UsbDisk{,4k,Huge,ReadOnly}`, `kernel/src/usb_gate.rs`, the seven
`xhci_*` gates, `esp_filesystem`, `esp_log_file`, `boot_partition_identity`,
and the harness behind them.

Verified at `8d7044c`. QEMU 11.0.2. Nothing in the repository was changed:
every breakage below was applied, run, and reverted inside one command, and
`git status --porcelain` was empty over the whole tree afterwards.

**Every number here came from a command that was run.** Baselines, all green:

| filter | result |
|---|---|
| `cargo test -- usb_storage` | 3 passed, 19.9 s |
| `cargo test -- xhci_` | 7 passed, 40.4 s |
| `cargo test -- esp_` | 2 passed, 13.9 s |
| `cargo test -- boot_partition_identity` | 1 passed, 6.5 s |

---

## Summary

Three demonstrated holes, one dead assertion, one tautology, and a set of
device shapes QEMU will stage today that no profile asks for.

- **`esp_filesystem`'s fsck gate is blind to any value in the `..` entries of
  `/EFI` and `/toyos`** — including an out-of-range cluster — because the
  pre-existing complaint about those two fields masks every further complaint
  about them. Demonstrated on the volume the test itself fscks, with the test's
  own masking logic. Those are the two directories holding `BOOTx64.EFI`,
  `kernel.elf`, `initrd.img` and `kernel.log`.
- **`usb_storage_gate`'s read half is certified by one in-guest comparator that
  nothing certifies.** Demonstrated: corrupt every read of block 1 in the driver
  *and* neuter `usb_gate::first_bad`, and the gate passes while printing
  `usb-gate: host block 1 verified`.
- **`xhci_no_interrupt`'s "nothing claimed a device" tooth passes on any absent
  line.** Demonstrated: reword the driver's bind log line and it stays green
  while `xhci_many_devices` reds.
- **`tests/common/usb.rs:277` can never fire** — the needle is
  `"usb-gate: disk designated"` and the kernel prints
  `usb-gate: disk {index} designated, …`.
- **`healthy=true`, asserted in both gate summary lines, is a constant.**
- The `stamp names its own block count` guard has no test: deleting it leaves
  the suite green.

Where the gates are strong, they are *very* strong, and that is as much of the
result: the write half's nonce interlock, `usb_storage_shapes`' huge-disk
refusal string (which covers both fields of READ CAPACITY(16) without saying
so), `xhci_msi_only`'s two closed vacuity routes, `esp_log_file`'s three
anti-vacuity arms, and `boot_partition_identity` all survived everything aimed
at them.

---

## Part 1 — per-gate verdicts

### `usb_storage_gate` — write half sound, read half vacuous under one mutation

**Sound: the write half.** Ground truth is the backing file on the host, and
the guest's bytes are keyed on `!nonce` where the nonce is read out of block 0
of the disk itself. A guest that never read block 0 cannot produce those bytes,
and a leftover image from an earlier run carries a different nonce. `verify()`
then checks both guest blocks and all nine run blocks at the LBAs they were
told to use, that block 0 still starts with the stamp, that the two host blocks
are unchanged, and that blocks 3, 13 and `blocks - 3` are still zero. This is
real ground truth and it survived every probe.

**Sound: the batching loop is really crossed.** `MSC_MAX_BLOCKS` is
`MSC_DATA_LEN / 4096 = 8` and `RUN_LEN` is 9, so the nine-block run is two SCSI
commands and the host checks all nine blocks. Live numbers from the baseline:
disk 0 is the boot stick at `60230 blocks of 512 B (235 MiB)`, disk 1 the data
stick at `8388608 blocks of 512 B (32768 MiB)`; the high host block is
`8388607`, whose sector LBA is 67,108,856 — 27 bits, as `USB_STICK_BYTES`'
comment claims.

**Vacuous: the read half, under a mutation of the in-guest comparator.**
Two runs:

| mutation | result |
|---|---|
| `msc.rs` `transfer_blocks`: `(*dst)[offset] ^= 0xFF` when `lba + done == 1` | **FAIL** — `the guest did not report a clean pass`; guest printed `usb-gate: host block 1 differs at byte 0: 0x73 not 0x8c` and `disk done reads=bad writes=ok refusal=true wr_err=0 healthy=true` |
| the same, **plus** `usb_gate::first_bad` replaced by `None` | **PASS** (11 s) — guest printed `usb-gate: host block 1 verified` and `reads=ok writes=ok`, harness printed `host bytes read, guest bytes verified host-side` |

The teeth exist, and they live entirely inside `kernel/src/usb_gate.rs`, which
is instrument code no test covers. The harness stages the bytes but never sees
what the guest read — the only report is a boolean the guest computes about
itself. `specs/device-test-strategy.md`'s rule ("the instrument is code, and it
will be wrong before the driver is") applies to this file and nothing in the
suite applies it.

**Dead: `tests/common/usb.rs:277`.**

```rust
if log.contains("usb-gate: disk designated") {
    return Err(format!("the gate claimed an unstamped disk\n{log}"));
}
```

`kernel/src/usb_gate.rs:111` is
`log!("usb-gate: disk {index} designated, blocks={blocks} nonce={nonce:#018x}")`,
so the rendered line is `usb-gate: disk 1 designated, …` for every possible
index and the needle has no slot for it. Counted over the baseline run:
**0 occurrences of the needle, 3 of `designated`.** The interlock itself is not
a hole — the `fingerprint()` comparison over the first 64 KiB and last 16 KiB
is real ground truth and is what would actually red — but this assertion is
structurally unfalsifiable. Fixing the needle to `" designated, blocks="` was
run and is green, so the correction costs nothing.

**Untested: the stamp's block-count guard.** `if false && stamped != blocks`
in `usb_gate.rs` leaves `usb_storage_gate` **green** (11 s). Every stamped image
this harness produces is stamped for its own size, so the guard that stops a
stamped image being written at offsets that mean something else on a
differently-sized disk has never executed.

**Tautology: `healthy=true`.** `UsbBlockDevice::healthy()` calls
`xhci::storage_geometry(index)`, which returns `MscDevice::geometry()`:
`logical_block_bytes` and `blocks`. It does not consult `failed`. `dev.blocks`
is assigned at exactly one site (`msc.rs:747`, in `bring_up`) and never again,
and nothing removes from `ctrl.storage`. So for every index `usb_gate` iterates
— it iterates `0..count()` — `healthy()` is `true` for the life of the boot,
and both asserted summary lines end `healthy=true` unconditionally. The doc
comment says it answers "what a caller asks after a run of failures";
`usb_storage_write_error` is exactly that run and the answer is still `true`.
(Reached independently by the code audit.)

**Missing: nothing checks the boot stick.** The gate's worst possible outcome
is a raw write to the disk the machine boots from, and the only evidence
against it is the *foreign* arm — an all-zero image, whereas the boot stick's
block 0 is a protective MBR. All three gate boots do reach
`esp: boot partition mounted, 246599680 bytes …` and `esp-log: this boot's
kernel log continues in /boot/toyos/kernel.log`, so the stick is provably
intact — but no assertion reads either line, and
`serial::Serial::must_be_clean()` only looks for the three panic markers.

### `usb_storage_shapes` — sound, with one covered claim worth recording

The 4 KiB arm is honest about the trap it is in: at 512 B and at 4096 B the
block count is the *same* number (`8388608` both times, confirmed in both
boots), so `check_geometry`'s `blocks of {lba} B` needle is the only thing
separating them. It reads the driver's own report, and the profile's declared
value reaches argv — but unlike `usb_storage_write_error`, which asserts
`readonly=on` in argv, this arm never checks that QEMU was actually told 4096.

The huge arm is stronger than it reads. The refusal string it requires,
`has 6442450944 sectors; this driver issues READ(10)`, can only be produced if
READ CAPACITY(16) decoded **both** fields correctly: offsets 0..8 must give
6,442,450,944 for the number to match, and offsets 8..12 must give 512, or
`bring_up` would have refused on block size first with a different message.
A shifted decode reds. That is not stated anywhere and is worth keeping.

Gap: the huge arm never fingerprints its 3 TB image, so a driver that wrote
before refusing would be invisible.

### `usb_storage_write_error` — sound

Three write calls, three refusals reported through the trait, and the live
evidence is three `usb-storage: SCSI 0x2a failed, sense 0x07/0x27/0x00` lines
followed by `disk done reads=ok writes=bad refusal=true wr_err=3 healthy=true`.
`wr_err` is the right shape: it counts refusals the *error channel* carried,
where `writes=bad` is true on this profile whether or not anything was
reported.

Two things the run shows that the test does not say:

- **`flush()` succeeds on the write-protected LUN** — there is no
  `usb-gate: the disk refused to flush` line — which is why the count is 3 and
  not 4. The flush error path is therefore unreached everywhere in the suite.
- The device-side error channel is exercised for WRITE(10) only. **No test
  makes a read fail at the device**; `refusal=true` comes from the driver's own
  `lba + count <= dev.blocks` bound and never reaches the wire
  (`usb-storage: 8388608+1 is past the 8388608 blocks this disk has`).

### `xhci_*` — all seven green; one assertion passes on any absent line

`xhci_no_interrupt`'s central tooth is

```rust
let binds = parse_xhci_binds(boot.text());
if !binds.is_empty() { … "a device was announced on a controller nothing can read" }
```

`parse_xhci_binds` looks for `xHCI: USB … ready on slot …, int_ring +0x…`, and
nothing inside this test uses the parser positively. Demonstrated: change
`device.rs:429` to print `xHCI: usb {} bound on slot …` and

| test | result under that rewording |
|---|---|
| `xhci_no_interrupt` | **PASS** (4.7 s) |
| `xhci_many_devices` | **FAIL** — `0 keyboards bound, want 2: []` |

So the suite as a whole catches the rewording; the assertion itself does not,
and it is the "passes on any `absent` line rather than the specific one" shape.
`boot.must_say("xHCI: 1 controller(s), 0 HID device(s)")` in the same test is
the count assertion that keeps it honest, and it is positive.

Records correction: `606efc9` says "with every log assertion deleted from
xhci_no_interrupt it still reds on `a device was announced on a controller
nothing can read`". That surviving assertion *is* a log assertion — it parses a
log line — as is the `xHCI: found at PCI ` count above it. The substance holds
(there is a tooth outside the `must_say`/`must_not_say` vocabulary); the
sentence as written overstates it.

`xhci_msi_only` — **both** vacuity routes its commit names are genuinely closed
by the current profile, verified by reading the code rather than by trusting
the commit. The storage controller is `msix=off,msi=off`, so `arm_interrupt`
returns `None`, so `init_one` refuses it and it never enters the `XHCI` vec —
it therefore cannot publish an `irq_ring` record for `poll_if_pending` to poll
every controller on, and with no USB storage bound there is no boot volume, so
`esp_log` never runs and there is no idle-loop cadence for `wait_transfer` to
drain HID reports on. Sound.

`xhci_many_devices` and `xhci_slot_exhaustion` — the DMA-layout assertions read
a *derivation* and not a printed number: `blocks == cap_slots` with the pool
proved four times larger, against `max_slots` taken off HCSPARAMS1. Live:
`xHCI: max_slots=64 max_ports=8 … dma 2048 KiB: scratchpad=0 device blocks=64
of 12288 B (max_slots=64)` on the default controller, and under
`xhci-one-slot`, `1 block of 12288 for 6 devices, 5 dropped, slot 1 addressed`.
Strong.

`xhci_second_controller`, `xhci_two_controllers`, `xhci_xecp_walk` — no hole
found. The first two assert an injected delta and a chord reaching userland
with the i8042 off, which no log line can fake; the third requires `8/8` rather
than the absence of a failure, and orders the handoff before `xHCI: controller
reset` by byte position in the log.

### `esp_filesystem` — the fsck gate is blind on the fields the baseline names

`fsck_complaints` compares *sets* of digit-masked lines. The 12 pre-existing
complaints, dumped by temporarily instrumenting the function (identical in all
four call sites — `esp-before`, `esp-after`, `esp-log-before`, `esp-log-after`):

```
Correct? no                                                 (x5)
Fix? no
Warning: Free space in FSInfo block is unset (should be #)
Warning: Item /EFI does not appear to be a subdirectory
Warning: Item /EFI/BOOT does not appear to be a subdirectory
Warning: Item /toyos does not appear to be a subdirectory
Warning: `..' entry in /EFI has non-zero start cluster
Warning: `..' entry in /toyos has non-zero start cluster
```

Six of the twelve are bare prompts carrying no identity, so the set that can
actually mask something is six description lines.

The volume was then dumped and mutated host-side, and each mutation re-scored
with the test's own masking, summary-stripping and `fresh` logic:

| mutation | fresh complaints |
|---|---|
| `/EFI` `..` start cluster 2 → 3 | **[] — invisible** |
| `/EFI` `..` start cluster 2 → 999999 (out of range) | **[] — invisible** |
| `/toyos` `..` start cluster 2 → 7 | **[] — invisible** |
| `/EFI/BOOT` `..` (currently correct) 3 → 9 | `` Warning: `..' entry in /EFI/BOOT has incorrect start cluster `` |
| `/EFI` `.` 3 → 12345 | `` Warning: `.' entry in /EFI has incorrect start cluster `` |
| `/EFI/BOOT` `.` 4 → 12345 | `` Warning: `.' entry in /EFI/BOOT has incorrect start cluster `` |
| `/toyos` `.` 1842 → 12345 | `` Warning: `.' entry in /toyos has incorrect start cluster `` |
| `BOOTX64.EFI`'s directory entry marked free (0xE5) | three fresh lines, including `Warning: Invalid long filename entry at end of directory /EFI/BOOT` |

The gate is sharp everywhere except the two fields the baseline already names,
where it is blind to *every* value. Those are the `..` entries of `/EFI` and
`/toyos` — the directories holding `BOOTx64.EFI`, and `kernel.elf` /
`initrd.img` / `kernel.log` respectively.

This is reachable code, not a thought experiment:
`fat32_adapter::ensure_parent` calls `Fat32::create_dir_all`, and the adapter
implements `rename`, whose directory case is `set_dot_dot`'s caller. A guest
bug that repointed either `..` would be invisible to `esp_filesystem` and to
`esp_log_file`. (A bug in a directory the guest *creates* would be caught — the
path differs, so the complaint is new.)

Two smaller properties of the comparison, both provable from the code and both
consistent with the measurements:

- `Vec::contains` discards multiplicity, so N copies after against one before is
  silent. On the current tree the before/after **multisets are identical**, so
  making the comparison count-sensitive costs nothing today.
- `mask_digits` collapses every digit run to `#`, so two complaints differing
  only in numbers are one complaint.

Positive: everything else in `esp_filesystem` is genuine host-side ground
truth — the host note staged into the image before the machine exists, the
41,097-byte blob compared byte for byte, `BOOTx64.EFI` / `kernel.elf` /
`initrd.img` compared against what the builder wrote, and the deleted file and
the refused symlink asserted *absent*.

### `esp_log_file` — sound, one gap

The three anti-vacuity arms all hold: the nonce is this image's own unique
partition GUID (`gpt: firmware booted us from partition {guid} `), which
`create_gpt_disk` draws fresh per build; the mid-run read happens before any
shutdown and must already carry `Boot: complete`, which is logged two phases
after `esp_log::install`; and the post-shutdown read must additionally carry
the shutdown's own last line. Measured this run: 6485 bytes on the device 18 ms
after the ready marker, 7319 after the shutdown.

Gap: **the rotation arm does not fsck the volume.** Rotation is the path that
deletes and renames — FAT has no atomic replacement, so the adapter deletes the
destination first — and it is the only ESP write path in the suite with no
filesystem-level check afterwards. Observed this run: 6 rotations, 259 bytes in
`kernel.log.1`, 0 in `kernel.log`. The gate requires ≥2 and is right to.

### `boot_partition_identity` — sound

Green (6.5 s). The positive arm requires `entry 2 of 3` behind an ESP-typed
decoy at entry 0, so neither an index nor a type GUID could have answered; the
ambiguity arm requires the machine to end with *no* boot volume when a second
device claims the same unique GUID; the negative arm requires the eight-block
shift to be refused **and** the real partition on the stick to survive it.
Ground truth is the `gpt` crate, a different implementation from the one under
test, and `craft_decoy_disk` re-reads its own output before the boot.

Gap: no arm puts the boot partition on the *second* USB disk. Every run matches
on `gpt: device 16`, i.e. USB index 0.

---

## Part 2 — states nobody stages

### QEMU 11.0.2 will stage these today

Each device line below was launched and quit cleanly with no error, so the
shape is available to the harness as it stands:

| shape | what it drives | tested? |
|---|---|---|
| `usb-storage,logical_block_size=8192` | `!matches!(block_bytes, 512\|1024\|2048\|4096)` in `bring_up` | **no** |
| `logical_block_size=1024` / `2048` | `sectors_per_block` of 4 and 2 | **no** (only 8 and 1) |
| three `usb-storage` on one controller | `MSC_BLOCKS = 2`'s refusal (`slot N is the 3th disk; this driver serves 2`) | **no** |
| `usb-bot` + `scsi-cd` with no medium | `READY_BUDGET_NS` expiry, and `peripheral != 0` | **no** |
| `usb-bot` with `scsi-hd` at LUN 0 and LUN 1 | the hardcoded `LUN 0` byte in the CBW; the driver never issues GET MAX LUN (no `0xFE` request anywhere in `msc.rs`) | **no** |
| `blkdebug::` as the drive backend | a device-side **read** failure | **no** |
| QMP `device_del` / `device_add` on a `usb-storage` | hotplug, and removal under active I/O | **no** |
| QMP `block_resize` | capacity changing after READ CAPACITY | **no** |

The first is the one that matters most: it is the exact twin of the divide-by-
zero `Profile::NvmeWideSector` was built for, the refusal exists in `msc.rs`,
and USB has no equivalent profile.

The third is worth its own sentence because `specs/device-test-strategy.md`
records that a *shortage* scenario is not always host-stageable and that xHCI
slot exhaustion needed a kernel feature as its actuator. The mass-storage
shortage is not like that: QEMU will attach as many `usb-storage` devices as
the controller has ports, so `MSC_BLOCKS`'s overflow is stageable with no
kernel feature at all — it is untested only because `Shape` cannot express two
data disks.

### QEMU cannot stage these; they would need a kernel feature as actuator

- A CSW carrying another command's tag (`bot`'s `csw_tag != tag` refusal).
- A CSW residue larger than the transfer (`scsi`'s self-contradiction refusal).
- A short-but-successful data phase (`Scsi::Ok { delivered } != bytes`).
- `Bot::Broken` at all, and therefore `reset_recovery` and `dev.failed`. Both
  `dev.failed = true` sites (`msc.rs:298` and `msc.rs:635`) are unreached: a
  grep of the full `cargo test -- usb_storage` log finds no `transport broke`,
  no `reset recovery failed` and no `never became ready`.
- A controller resetting mid-transfer.

### Harness shape gaps, independent of QEMU

- **`Shape` carries one `usb_disk_*` triple**, so two data disks cannot be
  expressed. That is also why "a stamped disk on each of two controllers" is
  unstageable, and therefore why `xhci::with_disk`'s cross-controller walk
  (`first += count`) has never run with a non-zero disk count on more than one
  controller. `MetalXhciSecond` puts the *only* stick on the second controller,
  which exercises `disk_base` but not the walk.
- **Every `BootOptions` in this harness uses `smp: 2`** (the tree's only other
  values are the audio configs' `smp: 1` and `AUDIO_SMP = [1, 8]`). The T14 has
  8. So no gate here exercises `XHCI.lock()` contention at width — an idle-loop
  `esp_log` flush on one CPU, `poll_if_pending` on another, and the gate's own
  commands.
- **A gate boot with zero USB disks** is unstaged: `gate_ran` requires a
  `carries no stamp` line, which such a boot cannot produce.

---

## Part 3 — gates to add, ranked

1. **Host-side cross-check of the read half.** Have `usb_gate` print a digest
   of every block it read and have the harness recompute it from the bytes it
   staged. Closes the demonstrated vacuity above at the cost of one log line;
   today a corrupted read plus a broken comparator is a clean pass.
2. **`Profile::UsbDiskWideSector` — 8192-byte logical blocks.** The USB twin of
   the `#DE` that `NvmeWideSector` exists for. One profile arm, one boot.
3. **Three data sticks on one controller.** Catches `MSC_BLOCKS = 2`'s
   overflow. Needs `Shape` to carry a list of USB disks rather than one, which
   also unlocks item 11.
4. **A `usb-bot` LUN pair.** Catches the missing GET MAX LUN and the hardcoded
   `LUN 0`: on a real multi-LUN device the driver silently binds one unit and
   reports its capacity as the device's.
5. **Close the `..` blind spot in `esp_filesystem`.** The right fix is to make
   the image builder write conformant `.`/`..` so the baseline is *empty* —
   then every complaint is fresh by construction and the whole masking problem
   goes away. Failing that, read the two entries of every directory directly
   and compare them before and after; `fsck_msdos` will keep reporting the
   pre-existing line no matter what the guest does to those fields.
6. **fsck the volume in `esp_log_file`'s rotation arm**, and make the
   comparison count-sensitive. Free today — the multisets already match.
7. **`usb-bot` + `scsi-cd` with no medium.** Two untested refusals, and the
   only path that would exercise `READY_BUDGET_NS` as a wall-clock budget
   rather than as dead code.
8. **`blkdebug` read-error injection.** The only way to reach a device-side
   read failure, `Bot::Broken`, `reset_recovery` and `dev.failed`.
9. **Assert the boot stick survived every gate boot** — `esp: boot partition
   mounted` present, and no new fsck complaints on the boot image afterwards.
   Costs one line each; today the gate's most dangerous possible outcome has no
   assertion at all.
10. **Fix or delete `healthy()`.** As written it is a constant, and the summary
    line asserts it. Either it consults `failed` or the field goes.
11. **A `smp: 8` variant of `usb_storage_gate`**, matching the machine that
    gets flashed.

Two records corrections, both above: `tests/common/usb.rs:277`'s dead needle,
and `606efc9`'s "every log assertion deleted".

---

## Part 4 — probes that came back clean

Stated because a clean probe is a result.

- The nonce interlock. There is no way to produce the guest's write pattern
  without a working read of block 0, and the harness recomputes it
  independently. Nothing weakened it.
- `verify()`'s must-be-untouched blocks (3, 13, `blocks - 3`) bracket both ends
  of every write the gate makes, so a batch that splattered a neighbour is
  caught at either end.
- `at()` in the kernel and `block_of()` in the harness are byte-for-byte the
  same function, so a negative index cannot mean two different blocks.
- `usb_storage_shapes`' huge-disk refusal covers both fields of READ
  CAPACITY(16), as argued above.
- `xhci_msi_only`'s two documented vacuity routes are closed by construction.
- `serial::Serial` is a genuinely good instrument: `must_not_say` requires the
  channel to have carried kernel output first, and `self_check` proves all ten
  cases in both directions with no guest.
- `boot_partition_identity`'s decoy disk is re-read with the outside
  implementation before the boot, which is the instrument certification
  `specs/device-test-strategy.md` asks for and almost nothing else does.
- `esp_filesystem` catches `.`-entry damage in all three directories, `..`
  damage in the one directory the baseline does not already name, and a
  directory entry marked free.
