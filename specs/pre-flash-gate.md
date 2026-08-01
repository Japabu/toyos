# Pre-flash gate — written go/no-go before the T14 boots

Run this before flashing anything to the ThinkPad T14. It is a **written
verdict**, not a test run: record each result and sign off, or do not flash.

**A green suite is not this gate.** A green suite says the tests pass, not that
the change is right — `specs/metal-track-history.md` records twelve
certifications that could not fail, and one session alone produced three more
(`specs/spec-staleness-sweep.md`, "Break it and run it"). Every item below
therefore has a **false pass** column: the way the check can report success while
the property is broken. If you cannot rule out the false pass, the item is a
no-go regardless of what the terminal printed.

Run on a **quiet tree** — no other agent building or booting. Contention
produces failures that look like defects and, worse, passes that look like
verdicts.

---

## What this gate does NOT cover

State these to the owner before he flashes. They are not gaps to be closed here;
they are things only the hardware can answer.

1. **Whether the T14's EC lands in scancode set 2 with translation on.** QEMU
   cannot decide it. The driver's `0xF0 0x00` read-back determines the wire
   format and refuses to attach to one it did not ask for, so the answer appears
   as one line on the laptop's own screen rather than as a bisect. **A refusal to
   attach is the driver working**, not a regression.
2. **The touchpad is I2C-HID and unbuilt.** A dead touchpad is the expected
   outcome. Do not treat it as a regression, and do not let it consume debugging
   time on the machine.
3. **Real-hardware performance.** TCG cannot measure the 2× bar. The T14 is the
   first honest instrument; nothing here substitutes.
4. **Anything the on-screen console cannot show.** Input is dead on that machine
   and there is no serial, so the console is the entire diagnostic channel. If it
   is broken, every other failure this boot becomes silent — which is why §4 is
   the gate's highest-severity section.

---

## 1. Storage: nothing may write to a disk it was not given

**The highest-consequence section.** The last boot was saved from reformatting
the owner's disk only by a bug since fixed.

**The structural problem: this cannot be caught by the harness.** Every scratch
image the harness creates *is* designated, so a regression that widens the write
path stays green through the entire suite and only misbehaves on a disk that is
not ours — his. **Defeating that property is what §1.1 and §1.2 exist to do.**
Do not substitute "the storage tests pass".

### 1.1 The designation stamp is still the only gate

| | |
|---|---|
| **Run** | Read `bcachefs/src/superblock.rs`. Confirm `DESIGNATION_MAGIC = b"TOYOS-FORMAT-ME\0"` at `:24` and `DESIGNATION_BLOCKS_OFFSET = 16` still exist, and that the `const` assert keeping `DESIGNATION_MAGIC`'s first four bytes distinct from `MAGIC` (`:30-33`) still compiles. Then `git log -p --since=<last flash> -- bcachefs/src/superblock.rs kernel/src/bcachefs_adapter.rs` and read **every** hunk. |
| **Expect** | The stamp is checked before any write path is reachable, and no commit in the range adds a second way to reach `format()`. |
| **False pass** | A new caller reaches a write path *without* consulting the stamp — the stamp is intact and simply no longer the only gate. Reading the constant proves the constant; only reading the callers proves the interlock. Also: a commit that makes the check permissive (warn-and-continue instead of refuse) leaves both the constant and the call site looking correct. |

### 1.2 `format()` and `mount()` are still private behind `probe`

| | |
|---|---|
| **Run** | `git grep -n "pub fn format\|pub fn mount\|pub fn probe" -- kernel/src/bcachefs_adapter.rs` |
| **Expect** | `fn format()` and `fn mount()` **without** `pub` (currently `:299`, `:306`); `pub fn probe()` (currently `:343`) the only public entry. |
| **False pass** | `pub(crate)` reads as "not public" to a skim but opens the whole kernel. Grep for `pub(` too. Equally: `probe` itself gaining a parameter that lets a caller skip the designation check — the signature is public API, so a new argument is a widened gate that this grep will not show. |

### 1.3 No write path bypasses `probe`

| | |
|---|---|
| **Run** | `git grep -n "raw_block_write\|write_block\|NvmeBlockDevice" -- kernel/src` and enumerate every caller. For each, establish it is downstream of `probe`. |
| **Expect** | Every writer is reached only via a `Storage` returned by `probe`. |
| **False pass** | An enumeration that covered only `kernel/src`. Per `specs/fork-lint-audit-plan.md`, **"I enumerated the call sites" is only true if the enumeration covered `~/.cargo/git/checkouts/`.** Check there too. |

### 1.4 Present the disk read-only for the first boot, if the owner accepts

| | |
|---|---|
| **Run** | Ask the owner whether the first boot can run with the internal NVMe physically or firmware-disabled. |
| **Expect** | A decision, recorded. |
| **False pass** | n/a — this is a recommendation, not a check. It is the only item that makes §1's failure survivable rather than merely unlikely, so record his answer either way. |

---

## 2. The image is flashable

### 2.1 Sector alignment

| | |
|---|---|
| **Run** | `cargo run -- --build-only`, then `ls -l target/bootable.img` and `python3 -c "import os;n=os.path.getsize('target/bootable.img');print(n, n%512)"` |
| **Expect** | Remainder `0`. `src/image.rs:111`'s `assert_eq!(total_size % 512, 0, ...)` should also still be present. |
| **False pass** | The assert covers the *computed* `total_size`, not the bytes actually written. If a later step appends or truncates, the assert passes and the file is still misaligned — **measure the file on disk, not the constant.** This broke once and the tail, including the backup GPT, silently never landed. |

### 2.2 The backup GPT is present at the very end

| | |
|---|---|
| **Run** | Read the last 512 bytes and confirm the GPT backup header signature (`EFI PART`). |
| **Expect** | Signature present in the final sector. |
| **False pass** | A primary GPT at the front makes `fdisk -l` look entirely healthy while the backup is missing — which is exactly the shape of the earlier breakage. Check the **tail** specifically. |

### 2.3 It still fits, and boots without the hosted rustc

| | |
|---|---|
| **Run** | Record image size. Then set `hosted-rustc = false` in `system.toml` (currently `true`, line 10), rebuild, and boot the resulting image under `cargo test -- --metal-sim`-equivalent profiles. **Revert `system.toml` afterwards.** |
| **Expect** | Boots to `Boot: complete` without the hosted rustc in the initrd. |
| **False pass** | Testing only the `hosted-rustc = true` image proves nothing about the smaller one, and a stale `target/` can leave the old initrd in place — confirm the image size actually dropped before believing the boot. |

---

## 3. Boot-time panics closed today are still closed

For each, the check is the **same shape**: confirm the guard exists *and* that a
test exercises the absent-device path. A guard with no test is a guard nobody has
seen fail.

### 3.1 NVMe absence

| | |
|---|---|
| **Run** | `cargo test -- diskless` (`Profile::Diskless`, `tests/common/qemu.rs:59`). |
| **Expect** | Boots to completion with no NVMe controller present. |
| **False pass** | The profile silently still attaching a disk. Confirm from the QEMU command line that no NVMe device is passed, not from the test name. |

### 3.2 `enable_fsgsbase` CPUID check

| | |
|---|---|
| **Run** | Read `kernel/src/arch/cpu.rs:231`. Confirm `enable_fsgsbase()` returns `bool` and that its caller handles `false` without executing an FSGSBASE instruction. |
| **Expect** | A CPUID check gating CR4.FSGSBASE (`:255`), and a caller that respects the result. |
| **False pass** | **QEMU's TCG always reports FSGSBASE**, so the `false` branch never executes in any test on any profile. This is untestable here by construction — verify by reading, and record it as read-verified rather than test-verified. The T14 has FSGSBASE, so the risk is the *check itself* faulting, not the fallback. |

### 3.3 Framebuffer extent

| | |
|---|---|
| **Run** | Read `kernel/src/drivers/gop.rs:39` and confirm the extent check is against `stride × height × 4`, not `width × height × 4`. |
| **Expect** | The check uses `stride`. |
| **False pass** | **On QEMU, `stride == width` on most modes**, so a regression to `width` is invisible in every emulated boot and only faults on hardware whose stride exceeds its width — which is the common case on real panels. Read the expression; do not infer it from a passing boot. |

### 3.4 xHCI with no HID device

| | |
|---|---|
| **Run** | `cargo test -- xhci` including the `MetalUsb` profile. Confirm `kernel/src/drivers/xhci/mod.rs:643`'s "no HID devices found" path logs and continues. |
| **Expect** | A controller with no HID on it is returned, not panicked on (`:636`). |
| **False pass** | Every USB profile in the harness attaches at least one HID, so the zero-HID path may not execute in any test that ran. Confirm the log line actually appeared in a boot, or that a profile with zero HID exists. |

---

## 4. The on-screen console — the only diagnostic channel

**If this section fails, do not flash.** With no serial and dead input, a broken
console means every other failure is silent and the boot is uninterpretable.

### 4.1 It renders on GOP

| | |
|---|---|
| **Run** | `cargo test -- screen` — the suite at `tests/toyos.rs:52-58`: `screen_decoder`, `screen_recoverable_untouched`, `screen_early_panic`, `screen_late_panic`, `screen_paged_scrollback`, `screen_panic_muted`, `screen_fatal_halt`. |
| **Expect** | All pass. These decode the framebuffer glyph-by-glyph against `font8x16.bin`; they are the only pixel-reading tests in the tree. |
| **False pass** | **`screen_late_panic` passes with `panic_console::capture`'s body replaced by `return`** — a known dead gate (`specs/known-issues.md`). So a green `screen` suite does **not** establish that capture works. Treat these as covering *rendering*, not capture. |

### 4.2 Scrollback is retained and pages on a timer

| | |
|---|---|
| **Run** | `cargo test -- screen_paged_scrollback`. Confirm against `3108e3a` ("Retain the log the screen shows, and page it without a keypress") that paging is driven by a timer. |
| **Expect** | Multiple pages rendered with no input. |
| **False pass** | A test that supplies a keypress, or one that asserts only the first page. **Input is dead on the T14** — if paging needs a key, the owner sees page one forever and nothing indicates more exists. Confirm the test injects no input at all. |

### 4.3 A fatal panic reaches the screen with no serial

| | |
|---|---|
| **Run** | `cargo test -- screen_panic_muted` (the `--mute` shape: metal-sim with the 16550 removed — the T14's literal configuration). |
| **Expect** | The panic report renders on the framebuffer with no serial present. |
| **False pass** | Passing under a profile that still has serial. Confirm the muted profile actually removes the UART, since this is the single most important behaviour on the machine. |

---

## 5. Today's changes do not alter boot behaviour

These are recent and unproven on hardware. Each check is "boot is unchanged",
not "the feature works".

| Change | Run | Expect | False pass |
|---|---|---|---|
| `cwd` bound | Boot and confirm init sequence completes | `Boot: complete`, all `system.toml` init programs start | The bound only bites over 256 bytes; a normal boot exercises none of it, so a pass says nothing about the fix. It is here to confirm **no regression**, not to validate the bound. |
| Ring modulus | `cargo test -- pipe` and `cargo test -- audio` (fast tier) | Unchanged from the last recorded run | Ring wrap needs sustained throughput; a short test never reaches the modulus. Compare against a recorded baseline rather than "it passed". |
| Poller tripwire | Boot with compositor + netd + soundd running | All three daemons reach steady state | The tripwire fires past `cq_size`; a quiet boot never approaches it. Confirms no regression only. |

---

## 6. Verdict

Record, per section: **pass / fail / read-verified-only**, and name who checked.

**Go only if:**

- §1 is pass, with §1.1–§1.3 confirmed by *reading callers*, not by a green suite.
- §2 is pass, with alignment and the backup GPT measured **on the file**.
- §4 is pass — no exceptions; it is the only channel that reports anything else.
- §3 and §5 are pass or explicitly recorded as read-verified with the reason
  (§3.2 and §3.3 are expected to be read-verified; QEMU cannot exercise either).

**No-go on any unresolved false pass**, even where the command printed success.
That is the entire purpose of this document.

State the three uncovered items from the top to the owner as expected outcomes
before he boots, so a scancode-set refusal or a dead touchpad is not mistaken for
a regression on a machine where debugging is nearly blind.
