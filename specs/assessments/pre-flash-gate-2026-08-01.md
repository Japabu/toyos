# Pre-flash gate — recorded verdict, 2026-08-01

Dated evidence, frozen: the recorded run of the pre-flash gate taken before
the T14's first flash. The reusable checklist this verdict was taken against
now lives in `specs/plans/metal-boot-plan.md`'s "Pre-flash checklist" section;
the `§1`–`§4` references below name that checklist's sections as they stood on
2026-08-01, at tree `b82fc4a` (§5A at the drafting commit, `7a52b13`). This
file is not maintained — see `specs/README.md` for how dated records are
treated once frozen.

## 5. Today's changes do not alter boot behaviour

These are recent and unproven on hardware. Each check is "boot is unchanged",
not "the feature works".

| Change | Run | Expect | False pass |
|---|---|---|---|
| `cwd` bound | Boot and confirm init sequence completes | `Boot: complete`, all `system.toml` init programs start | The bound only bites over 256 bytes; a normal boot exercises none of it, so a pass says nothing about the fix. It is here to confirm **no regression**, not to validate the bound. |
| Ring modulus | `cargo test` inside `toyos-abi/` (~13 s), plus `cargo test -- pipe` and `cargo test -- audio` (fast tier) for no-regression | The three host tests pass, `a_stream_survives_the_cursor_reaching_two_to_the_thirty_two` among them; the guest suites unchanged | **The original wording — "unchanged from the last recorded run", "compare against a recorded baseline" — is not a check: no baseline for `pipe` was ever recorded, and a pass/fail test has no distribution to compare.** Struck and replaced. The real coverage is the host tests, which push 4 GiB through one ring and cross the new wrap ~32,800 times; the guest suites only confirm no regression, because ring wrap needs sustained throughput a short test never reaches. |
| Poller tripwire | Superseded — see §5A, item 3 | | §5 describes `697072e`, step 1 of 3, which made the dropped completion *loud*. `414f5fb` then made it unrepresentable and turned two cases into panics. Certifying the old behaviour would be certifying code that is gone. |

---

## 5A. Changes that landed after this gate was drafted

§5 is a snapshot of the tree at `7a52b13`, the commit that drafted the gate.
Eight commits touching the kernel, the build, or the boot path landed after it.
**A gate that certifies a tree which no longer exists cannot fail** — the exact
defect shape the rest of this document exists to catch, pointed at the safeguard
itself. So these are added as items rather than folded into §5, so the gate says
what it checked.

Same rule as §5: each check is "boot is unchanged", not "the feature works".

**Standing rule, because this section is itself perishable.** Before flashing,
run `git log <the commit this section last covered>..HEAD --name-only` over
`kernel/ bootloader/ src/ toyos-abi/ toyos/ userland/` and add an item for
anything new. §5 went stale in under two hours; this section went stale once
*while being written*, which is how 5A.8 came to exist. A section that lists
commits by hash tells you exactly when it stopped being true — that is the only
reason to write it this way.

### 5A.1 Artifact staging — `9ee156c`

This one changed how **the artifact the owner flashes is produced**, so it
outranks the rest of this section.

| | |
|---|---|
| **Run** | On a quiet tree: `cargo run -- --build-only`. Then search `target/bootable.img` for the root `system.toml` init string (`/bin/compositor;/bin/soundd;/bin/netd` — `locale --load` led it until the layout syscall was deleted, and `/bin/sshd` trailed it until it was taken out of the default boot) **and** for `/bin/test-runner`. Confirm `target/kernel-*` and `target/bootloader.efi-*` staged copies exist. |
| **Expect** | The root init list present, `test-runner` absent, staged copies present. |
| **False pass** | The init list is **compiled into `bootloader.efi`** (`bootloader/build.rs` declares `rerun-if-env-changed=INIT_PROGRAMS`), and cargo keyed the artifact path on `(crate, target, profile)` and nothing else — so before this commit an image built while a `cargo test` ran got *another config's bootloader* with a plausible initrd. That failure is invisible to a size check, to `fdisk -l`, and to §2 entirely: the image is well-formed, it just boots the wrong init list. Two consequences. First, checking only that the right string is *present* passes on an image that also contains the wrong one — assert `test-runner`'s **absence** too. Second, **build on a quiet tree**: a concurrent harness run is the precise condition this commit fixes, so building under contention tests the fix rather than the artifact. |

### 5A.2 A CPU declines to halt while the log ring is non-empty — `a8a7204`

| | |
|---|---|
| **Run** | Read `log_ring::drain_chunk_to_serial` and `serial::uart_write_bytes`. Confirm the drain calls `drain_into` (which advances `tail`, decrements `len` and stores `OWED`) **before** `backend.write_raw`, and that `uart_write_bytes` returns immediately when `!uart_present()`. Then `cargo test -- screen` and `cargo test -- audio`. |
| **Expect** | On a machine with no serial the drain still empties the ring, so `has_pending()` self-clears and the idle CPU halts on the next trip round the loop. |
| **False pass** | **No test observes a muted, idle machine — which is the T14's steady state.** `screen_panic_muted` is the only profile with no console at all, and it panics rather than idling; every profile that idles has a console draining the ring for it. If the drain did not consume, all cores would spin at 100% on the laptop and nothing in this suite would show it. Record this item as **read-verified**, not test-verified, and say which read: `drain_into` consumes unconditionally and `uart_write_bytes`'s first line is `if !uart_present() { return; }`. Note also that `LogRing::retained` is independent of draining, so the on-screen scrollback survives this change — CLAUDE.md's "the ring is drained continuously, so there is no scrollback behind it" predates `3108e3a` and is stale. |

### 5A.3 Poller capacity is now enforced by panic — `414f5fb`, `8edbd5b`

Supersedes §5's "Poller tripwire" row.

| | |
|---|---|
| **Run** | `cargo test -- poller_capacity`. Boot with compositor + netd + soundd and confirm all three reach steady state. Read every `Poller::new` argument in `userland/`. |
| **Expect** | No `Poller::new:` panic. Compositor `3 + (MAX_HANDLES - 3)`, netd `2 + (MAX_HANDLES - 2)`, soundd `64` and `MAX_CONTROL_CLIENTS + 1 = 64`, terminal `4`, window `1`. |
| **False pass** | Compositor and netd sit **exactly on** `MAX_HANDLES = 256`, so a boot with either one alive says the boundary is inclusive and nothing more. Both sums are `const` and derived from `MAX_HANDLES` itself, so they cannot exceed it by construction — which means a green boot certifies nothing about the new panic. The only caller whose count is not first-party is `libc`'s `poll()`; confirm it returns `-1`/`EINVAL` above `MAX_HANDLES` **by reading `userland/libc/src/posix_io.rs`**, not by inferring it from a quiet boot. |

### 5A.4 `create_dir` returns a `Result` the boot path `expect`s — `781f2d6`

| | |
|---|---|
| **Run** | Read `kernel/src/main.rs`'s two boot-path `create_dir` callers. Then `git ls-tree HEAD rust` and compare against the `rust/` submodule's own HEAD. |
| **Expect** | `/home/root` and `/home/root/.config` created; boot completes. The pin is `3cd2144` ("std: toyos getcwd must not silently truncate the cwd") and matches the submodule. |
| **False pass** | Two **new `.expect` sites on the boot path**, and a boot proves only that two kernel literals are under `MAX_PATH` (4096) — which they are by four orders of magnitude. The item that can actually fail is the other half: `SYS_GETCWD`'s return contract changed in the same commit, with the matching std change in the **submodule**, not this repo. A stale pin gives a std that mis-reads the new return, and **nothing on the boot path would say so** — `current_dir()` would quietly hand back a valid-looking path to a different directory. Check the pin; do not assume the submodule moved with the commit that needed it. |

### 5A.5 ELF loader bounds what it *derives* — `b554798`

Runs for every program spawned and every `dlopen`, so every boot exercises it.

| | |
|---|---|
| **Run** | Boot and confirm every `system.toml` init program starts. `cargo test -- abuse_elf_loader`. |
| **Expect** | All init programs spawn; the relocation prescan still caches `libtls_cranelift.so`. |
| **False pass** | The flashed `system.toml` is five init programs and none of them is the case this commit nearly broke: the commit records that a bound taken on total entry count would have **silently** put the largest library in the tree (211 K relocation entries, 77 stored) back on the scan-every-clone path. A silent fallback to a slower path is invisible to "all programs started". Look for the cache line or record it as unmeasured. |

### 5A.6 libc gives `FILE` a buffer — `8de0a95`, `14120a5`

| | |
|---|---|
| **Run** | Any boot whose assertions read console text. Confirm the buffer is flushed on process exit. |
| **Expect** | Console output unchanged in content. |
| **False pass** | This change *improves* the known line-atomicity defect (kernel `log!` interleaving with userspace `println!` mid-word), so a suite that was intermittently red on interleaving can go green for a reason unrelated to what is being certified — read a green run as weaker evidence than usual, not stronger. The failure it introduces instead is a **lost tail**: a program that exits without flushing loses its last partial line. On the T14 the screen is the only channel, so a lost tail is a lost diagnostic. |

### 5A.7 The window protocol can say no — `8529cb3`, `8edbd5b`

| | |
|---|---|
| **Run** | `cargo test -- window`, `cargo test -- metal_sim_window_caps`, `cargo test -- compositor`. |
| **Expect** | A client survives a compositor that refuses. |
| **False pass** | **This certifies almost nothing about the flash.** The T14 boots the compositor with no client until a terminal exists, and input is dead on that machine, so no window is ever created and the refusal path is never reached. Its value here is only "the compositor still starts". |

### 5A.8 netd's capacity refusal and its new error code — `dd91b14`

Landed while §5A was being written, which is why the standing rule above exists.

| | |
|---|---|
| **Run** | `cargo test -- netd`, and confirm from a metal-sim boot that netd still exits gracefully on a machine with no NIC. |
| **Expect** | `netd_caps` passes on `tests/netcase`; under metal-sim netd finds no device and exits, unchanged. |
| **False pass** | **The changed code cannot execute on the T14 at all.** netd's `main` opens the NIC and returns on `NotFound`, so on a machine with no NIC — metal-sim by design, and the laptop, which has no driver for its hardware — not one line of the new cap, the refusal sites or the widened poller runs. `tests/netcase` is the *only* config that puts a NIC in front of netd, so a green `netd_caps` is evidence about a configuration the owner is not flashing. What this item actually checks is the **unchanged** half: that netd still exits rather than panicking, which `metal_sim_compositor` reads from the daemon's own words. Do not let a green network gate read as coverage of the flashed machine. |

---

## 7. Recorded verdict — 2026-08-01, tree `b82fc4a`

**GO.** Executed by an agent that did not write the gate, which is the point:
the author reads what they meant rather than what they wrote. Every item was
run, and for every item the false-pass question was asked explicitly and
answered. Run on a quiet tree, after killing one orphan QEMU (pid 92876,
reparented to PID 1, its harness gone, 7.6 s of CPU in 60 minutes elapsed and
not advancing — established *before* killing it, since a still-progressing
guest is somebody's measurement).

Guest suite **182/182 in 197.5 s**, timings scattered from 2 ms to 13 s, so the
"uniform timings mean one shared cause" diagnostic was not needed.

| Section | Result | How |
|---|---|---|
| §1.1 | pass | read; the range holds one commit, `5dff9aa`, the interlock itself |
| §1.2 | pass, wording corrected | read; three public entries, not one |
| §1.3 | pass | read + `~/.cargo/git/checkouts/` swept, 0 hits across 15 forks |
| §1.4 | **unanswered** | owner's decision |
| §2.1 | pass | measured on the file |
| §2.2 | pass | measured on the tail |
| §2.3 | pass, better than partial | see below |
| §3.1 | pass | `diskless_boot`, plus the argv |
| §3.2 | read-verified | QEMU always reports FSGSBASE |
| §3.3 | read-verified | QEMU's `stride == width` |
| §3.4 | pass, empirically | the log line, not the test name |
| §4.1 | pass, for rendering | capture is out of scope; see below |
| §4.2 | pass | no input on either side |
| §4.3 | pass | argv asserted against the same builder the boot uses |
| §5 | pass, one row struck | no `pipe` baseline ever existed |
| §5A.1–.8 | pass | §5A.2 read-verified |

### The artifact

Verified and left at **`target/bootable-diet.img`**, **123,076,608 bytes**,
sha256 `9bda620dc29f26445d6008f56eeeb3b11a9eaa79af8b0268301e8f43fbe531aa`.
Built with `hosted-rustc = false`; `system.toml` reverted afterwards and
`git diff` confirmed empty, and `target/bootable.img` rebuilt so it matches the
committed config again rather than being left as a stale image contradicting
it.

§2.1, §2.2 and §5A.1 were re-run **against that file**, because all three are
artifact-specific and results from a differently-configured sibling do not
transfer: 240,384 whole sectors (`% 512 == 0`), `EFI PART` in the final sector,
the root init string present exactly once, and `test-runner` and
`librustc_driver` absent entirely.

### §2.3, resolved rather than waived

The item cannot be executed as written — `hosted-rustc` lives in the root
`system.toml`, and every harness boot builds from `tests/metalcase/` or
`tests/testcases/`, neither of which carries the key. But its *substance* is
already covered: the harness images have **never** contained the hosted rustc
(the metal-sim initrd is 17 entries with no rustc; the `true` image carries
`bin/rustc` and a 207 MB `librustc_driver`). So every metal-sim boot in the
suite is already a no-hosted-rustc boot. Flipping the flag moved the image
674,885,632 → 123,076,608 bytes, which is what rules out the stale-`target/`
false pass the item warns about.

### Three findings to carry forward

**The interlock holds by enumeration, not by construction.** Every write path
was traced and every one is downstream of `probe` — but `PageCacheBlockIO` is a
`pub` unit struct with a `pub` trait impl, and `page_cache::raw_block_write` is
`pub`, so any future kernel module can write blocks without consulting the
stamp. Nothing does today. That is a fact about this tree at this commit, not a
property of the design, and it is why §1.1's "read every hunk" instruction has
to be obeyed on every future flash rather than trusted once.

**`capture` being a no-op no longer implies a lost report.** The dead gate is
still open (`specs/issues/panic-path/panic-console-capture-untested.md`: `screen_late_panic` passes with `capture`'s
body replaced by `return`), but `daabd3c` established that capture's original
reason is gone — retention (`3108e3a`) means `live_tail` after the flush
returns the same text. So the consequence of the untested path has changed from
"blank screen" to "shifted window", the residual being a sibling CPU pushing
more than 32 KiB through the ring between the panic and the paint. Unlikely,
unstaged, and bounded. §4.1 therefore passes for *rendering*, which is what the
item claims, with capture explicitly out of scope.

**CLOSED — `sshd` was in the root init list and in no test config.** It is in
neither now. The default boot does not start it, and `src/build.rs`'s
`no_shipped_boot_config_starts_sshd` is the gate on that; `tests/sshdcase` is a
boot that runs it with a NIC under it, the only config where it reaches a line
past its bind. What made the gap urgent rather than untidy was what the daemon
did once reached — it accepted every password and every public key it was
offered — which is fixed separately. The flashed machine now starts three
programs at boot and every one of them is spawned at boot by this tree.

`locale --load` used to be the second such program and is gone: the layout is a
config file every translator reads for itself, so there is nothing to load into
and nothing to run at boot.

### What could not be checked

1. **§2.3's boot half.** Nothing in this tree boots the root image; it needs
   `cargo run`. Inspection is all there is, which is why the inspection above is
   of that exact file.
2. **§1.4.** The owner's answer. It is the only item that makes a §1 failure
   survivable rather than merely unlikely, so it should be answered before the
   boot even though its absence is not a fail.
3. **A muted, idle machine** — the T14's steady state. The only console-less
   profile panics rather than idles, and every idling profile has a console
   draining for it. §5A.2 is read-verified for this reason.

### The four things the owner is signing up for

1. **Whether his EC lands in scancode set 2 with translation on is unknown, and
   only the hardware answers.** The driver's `0xF0 0x00` read-back decides the
   wire format and refuses to attach to one it did not ask for. **A refusal to
   attach is the driver working correctly** — one line on his own screen rather
   than a bisect.
2. **The touchpad is I2C-HID and unbuilt. A dead touchpad is the expected
   outcome**, not a regression, and it should not consume debugging time at the
   machine.
3. **Real-hardware performance is unmeasured.** TCG cannot test the 2× bar; the
   T14 is the first honest instrument.
4. **The artifact he flashes is booted by nothing in this tree.** Verified by
   inspection only — size, alignment, backup GPT, and the compiled-in init
   string.
