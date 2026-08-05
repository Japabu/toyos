# Test cost audit

What the suite costs, where the seconds actually are, and what each way of
spending less would buy — priced against measurements, with the risk of each
stated. **This audit optimises the suite's cost and never its coverage.** No
proposal here removes an assertion, weakens a negative gate, or drops a machine
shape; where a change could do that by accident, the section says what evidence
keeps its teeth.

Measured 2026-08-03 on the dev laptop (14 cores, `hw.ncpu` = `hw.physicalcpu` =
14). Two kinds of number appear and are always labelled:

- **Archived** — mined from nine complete full-run logs written by other agents
  between 2026-08-01 and 2026-08-03, and from 44 full-suite result lines in this
  session's transcripts. Other agents were building and booting throughout, so
  absolute values carry contention noise; *rankings* and *ratios within one run*
  are sound.
- **Isolated** — run by this audit, foreground, on a host with no other cargo or
  QEMU process active (`uptime` load 3.78, one leaked `qemu-system-x86_64` from
  a `cargo run` two days earlier idling at 1.7%). Still not a quiet host in the
  `tests/audio-baseline.toml` sense.

Anything derived by arithmetic rather than run is marked **(derived)**.

---

## 1. Where the seconds go

### 1.1 The suite is four blocks, and two of them are the whole cost

`tests/toyos.rs`'s `main` runs four blocks in order: a **shared boot** carrying
every Rust and C test, then **audio**, then **machine**, then **screen**. Only
the first shares a QEMU; everything else owns one.

Current census, from `cargo test --test toyos-build -- --list` (isolated,
2.9 s wall — see §1.3):

| block | tests | boots |
|---|---:|---:|
| Rust (shared boot) | 45 | shares 1 |
| C (shared boot) | 108 | shares 1 |
| audio | 2 names × 2 SMP = 4 | 4 |
| screen | 13 | ~14 |
| machine | 60 | ~57 |
| **total** | **229** | **~76** |

That census is already stale. `MACHINE_TESTS` held 60 names when this audit
started at 14:31 and 62 when it finished — `iommu_context_absent` and
`iommu_empty_domain` were added by another agent while it was being written.
**The suite grew during the audit of the suite.** That is the discipline working
as intended and it is why this document prices the cost curve rather than the
snapshot.

Per-block time, from the most recent archived full run (`full4.log`, 225 tests,
625.6 s suite):

| block | tests | seconds | share |
|---|---:|---:|---:|
| Rust | 44 | 10.9 | 1.7% |
| C | 109 | 2.6 | 0.4% |
| audio | 4 | 34.0 | 5.4% |
| machine | 55 | 407.0 | 65.1% |
| screen | 13 | 166.0 | 26.5% |
| inter-test / setup | — | 5.1 | 0.8% |

The same shape holds across all nine archived runs; machine has been 63–79% of
every one of them.

**The headline: 153 of 229 tests — 67% of the suite — share one boot and cost
13.5 s between them, 2.2% of the wall clock. The other 76 tests own a boot and
cost 97%.** Any proposal that touches the shared block is optimising 2% of the
problem.

### 1.2 Top ten most expensive tests

From the two most recent archived runs, which were launched two minutes apart
and overlapped almost entirely — so the pair also shows what contention does to
a number (225 tests both, 625.6 s and 596.0 s):

| # | test | full4 | suite4 |
|---:|---|---:|---:|
| 1 | `screen_console_scroll` | **103 s** | **109 s** |
| 2 | `double_fault_stack` | 25 s | 24 s |
| 3 | `xhci_deaf_registers` | 16 s | 14 s |
| 4 | `metal_sim_compositor_stall` | 14 s | 15 s |
| 5 | `iommu_discovery` | 14 s | 14 s |
| 6 | `i8042_mouse` | 14 s | 13 s |
| 7 | `log_backing_read_error` | 14 s | 12 s |
| 8 | `usb_flush_optional` | 13 s | 11 s |
| 9 | `i8042_health` | 12 s | 12 s |
| 10 | `usb_storage_gate` | 12 s | 11 s |
| — | `audio_tone_load` (smp=1, smp=8) | 10 s each | 10 s each |

`screen_console_scroll` alone is **17% of the entire suite** and four times the
next test. §3.1 shows most of that is host-side, not guest-side.

### 1.3 The phase split of a single own-boot test

Three isolated filtered runs decompose the fixed cost of owning a boot:

| what was run | what it does | suite time | wall |
|---|---|---:|---:|
| `serial_vocabulary` | pure host check, no image, no guest | 0.000083 s | 2.83 s |
| `log_partition_layout` | builds a boot image, reads its GPT, **never boots** | 1.4 s | 4.31 s |
| `nvme_wide_sector` | builds an image and boots a guest that dies at 0.068 s | 3.7 s | 6.52 s |

Reading down the column:

- **Prepare phase = 2.83 s.** Compiling 120 C tests and 60 Rust test binaries,
  warm. Paid once per run, and it sits *outside* `suite_elapsed` — the number
  printed as `test result: ok. N total (Xs)` does not include it. Negligible.
- **Image build = 1.4 s.** Paid on **every** boot.
- **QEMU spawn + OVMF + kernel boot + teardown = 2.3 s** (3.7 − 1.4). This is
  for a guest that panics 68 ms in, so it is pure emulator and firmware
  overhead, not guest work.

**The fixed tax for owning a boot is ~3.7 s. At ~76 boots that is ~281 s —
about 47% of a 600 s run is spent starting machines, not testing them.**
(derived from the two measured components × the counted boots)

### 1.4 The image build is three no-op cargo invocations

`build_test_image` (`src/build.rs:581`) runs `cargo_build` for the kernel, for
the bootloader, and for the userland workspace, then assembles the initrd and
the GPT/FAT32 image. Isolated benchmark of the assembly half, using the same
`fatfs` and `gpt` crates on a 64 MiB FAT32 volume with three multi-megabyte
files, in its own target dir so the shared tree was untouched:

```
opt-level 0: 52.1 ms per image build
opt-level 2:  4.0 ms per image build
```

So **assembly is not the 1.4 s** — the 1.4 s is three `cargo` process spawns
asking whether anything changed. `tests/testcases/system.toml` declares three
programs, all workspace members, so it really is three invocations.

**And most of those builds are redundant.** The image is a function of
`(config, kernel_features)` only. Across the 72 boots this audit could attribute
statically there are just **31 distinct image variants**:

```
27x testcases/plain          7x testcases/usb-storage-gate
 5x metalcase/plain          3x testcases/test-late-panic
 3x testcases/i8042-trace    2x console/test-screen-graffiti
 … 25 more, one boot each
```

**41 redundant image builds × 1.4 s = 57 s per run.** (derived)

### 1.5 Kernel feature rebuilds explain the 596 → 752 spread

There are **31 distinct kernel feature sets** in the suite (30 named plus
plain): `test-late-panic`, `i8042-trace`, `xhci-deaf-controller`,
`usb-storage-gate`, `test-tiny-va`, `test-heap-ceiling`, `fat-backing-read-fails,
test-small-caches`, and so on. `MACHINE_TESTS` and `SCREEN_TESTS` already sort
feature-carrying tests last to limit the thrash.

Two isolated A/B pairs measured what a feature set costs the *first* time it is
built against the current source:

| | first build of that variant | repeat |
|---|---:|---:|
| `heap_ceiling_recovery` (`test-heap-ceiling`) | 9.5 s suite / 26.9 s CPU | 4.5 s / 6.7 s |
| `va_exhaustion` (`test-tiny-va`) | 10.4 s suite / 25.9 s CPU | 6.4 s / 6.2 s |

**A cold kernel variant costs ~4–5 s wall and ~20 s CPU.**

Then a four-iteration alternation between `diskless_boot` (plain) and
`va_exhaustion` (`test-tiny-va`) showed **no rebuild at all** — 5.6–6.5 s CPU
every time, no 25 s spike. Cargo keeps every feature variant simultaneously:
`ls target/kernel-*` shows **36 staged artifacts**, one per feature key, and the
uplifted binary changes size between runs as cargo swaps the cached variant in.

That gives the model:

- **After any kernel source change**, a full run pays ~31 cold variants ×
  ~4–5 s ≈ **124–155 s** that a run on an unchanged kernel does not pay.
  (derived)
- **On an unchanged kernel**, feature switching is free.

This is the single largest explanation for the spread the owner is seeing across
otherwise-identical runs (596 s, 609 s, 706 s, 752 s). It is not noise and it is
not growth — it is whether that run happened to be the first after someone
touched `kernel/`.

### 1.6 Cost model, checked against the observed total

| component | seconds |
|---|---:|
| QEMU + OVMF floor, 76 boots × 2.3 s | 175 |
| image builds, 76 × 1.4 s | 106 |
| shared-boot guest work (45 Rust + 108 C) | 14 |
| audio, 4 boots | 34 |
| `screen_console_scroll` | 103 |
| everything else's guest work and host waits | ~170 |
| **subtotal, unchanged kernel** | **~600** |
| cold kernel variants, when `kernel/` changed | +124–155 |

Which brackets every archived full run.

### 1.7 What a new test costs — the actual growth rate

Two archived runs 47 hours apart, both full, both green:

| when | tests | suite |
|---|---:|---:|
| 2026-08-01 13:25 (`gate/full2.log`) | 202 | 360.0 s |
| 2026-08-03 12:17 (`full4.log`) | 225 | 625.6 s |

**+23 tests, +265.6 s — 11.5 s per added test.** (derived)

That is three times the 3.7 s per-boot floor, and the gap is the point. New
tests are not going into the shared boot; every one of the 23 went into
`MACHINE_TESTS` or `SCREEN_TESTS`, and several brought a new kernel feature with
them, which adds ~4–5 s of cold rebuild (§1.5) on top of the 3.7 s floor on
every run after a kernel change. **A gate written the house way currently costs
~11.5 s of everyone's suite, forever.** §3.1–3.3 do not change that marginal
rate much; §3.3 divides it by the parallel width, which is the only lever that
does.

**One post-wave-5 instance, and it says the marginal rate is now a different
question.** `doom_sound_flood` (2026-08-05) is a `Sched::Parallel` machine test
on a config of its own — `tests/doomcase`, doom beside soundd: **28 s of one
worker's time inside a 602.6 s wide phase**, which is ~2.3 s of suite wall at
width 12 and nowhere near the phase's longest job. Its own initrd and doom's
cold C build are paid once per tree, not per run. So what a new test costs is
no longer a per-test constant but the question of whether it lands on the
critical path, and for a wide phase that is the single longest job in it. Three
other worktrees' suites were on the host throughout that run, so its 602.6 s is
not comparable to any quiet-host figure here — the 28 s is, because nothing in
the test waits on a wall clock.

---

## 2. Classification

Every test, by what it actually needs.

### (a) Host-testable logic wearing a QEMU boot — **3 tests, already migrated**

`serial_vocabulary` (67 µs), `screen_decoder` (0 s) and `log_partition_layout`
(1.4 s, builds an image and never boots) already sit in `MACHINE_TESTS`/
`SCREEN_TESTS` while needing no guest. They cost nothing and their placement is
a registration convenience, not a defect.

**This audit found no further candidates.** Every other own-boot test asserts on
a running guest, on the host side of a device the guest touched, or on pixels a
guest painted. The 108 C tests and 45 Rust tests test programs *running on
ToyOS* — a host port would be testing something else. `toyos-sched/`,
`toyos-ps2/`, `toyos-gpt/` and `toyos-fat32/` already hold the logic that could
come down the ladder.

**Do not spend a wave here.** It is the option that sounds most attractive and
has the least in it.

### (b) Needs a boot but could share one — **12 boots, ~44 s**

Grouping all 72 attributable boots by the *full* `BootOptions` key — config,
profile, `kernel_features`, `i8042`, `mute`, `smp`, `ready_marker`,
`nvme_image`, `boot_image`, `usb_image` — gives **60 distinct shapes**. Only
these groups boot the same machine more than once:

| boots | shape | tests |
|---:|---|---|
| 4 | `metalcase` / `Metal` / plain | `metal_sim_compositor`, `metal_sim_compositor_stall`, `metal_sim_ipc_hostile_peer`, `metal_sim_window_caps` |
| 3 | `testcases` / `Metal` / plain, staged `boot_image` | `esp_filesystem`, `kernel_log_file`, `log_partition_identity` |
| 3 | `testcases` / `Metal` / `i8042-trace` | `i8042_keyboard`, `i8042_mouse`, `i8042_no_spurious_wake` |
| 2 | `console` / `Metal` / `test-screen-graffiti` | `screen_console_clear`, `screen_console_scroll` |
| 2 | `testcases` / `Gop` / `test-late-panic` | `screen_late_panic`, `screen_paged_scrollback` |
| 2 | `testcases` / `MetalHotplug` / plain | `xhci_flap`, `xhci_hotplug` |
| 2 | `testcases` / `Headless` / plain | `iommu_discovery`, `metal_sim_input` |
| 2 | `testcases` / `Metal` / plain | `i8042_health`, `i8042_undecoded_bytes` |

**12 boots removable, ~44 s (7%).** Note this is half what a coarser key
suggests: grouping on `(config, profile, features)` alone shows 23 removable
boots, and the difference is entirely `i8042: false`, `mute`, staged images and
`ready_marker` — fields that *are* the shape for the test that sets them.
Consolidating on the coarse key would silently change what several tests boot.

### (c) Genuinely needs its own machine shape — **~60 boots**

The remainder. `tests/common/qemu.rs` declares **21 `Profile` variants**, each
with a doc comment naming the defect that shape exists to reach — device size,
sector size, device presence, device *order*, link speed, two controllers,
hotplug. Per `specs/device-test-strategy.md` these are the suite's crown jewels
and none of them is a candidate for anything in this document.

### (d) Timing-sensitive — **~10 boots, and they constrain everything**

| test | what makes it timing-sensitive |
|---|---|
| `audio_tone` ×2, `audio_tone_load` ×2 | gate A's fast tier; see below |
| `i8042_fadt_denial` | compares two boots' guest `boot_millis` with a 300 ms margin (`tests/toyos.rs:4535`) |
| `metal_sim_pointer_churn` | QMP event pacing, 60–800 ms sleeps, margin sized for "QEMU coalescing rel events it has not been polled for" |
| `xhci_slow_connect` | mirrors the kernel's `SLOW_CONNECT_NS` as `HELD_EMPTY_S = 0.300` |
| `late_storage_connect` | same shape, `xhci-slow-storage-connect` |
| `xhci_flap`, `xhci_hotplug` | debounce windows |
| `i8042_health_cadence` | `i8042-fast-health` cadence |

**Gate A is the binding constraint, and it is not a margin question.**
`tests/audio-baseline.toml` records that its numbers were derived on "a quiet
host: one QEMU at a time, no concurrent agents or builds for the whole session".
The fast tier's per-run ceilings have headroom (`max_wake_lat_us = 56000` against
a 7 ms median) but the thorough tier compares *distributions* against that
recording, and `specs/audio-gate-history.md`'s reusable lesson is that these
counters drift between batches on one host with no code change. **Gate A cannot
be certified on a host that is running anything else.** Everything in §4 has to
answer to that.

---

## 3. Strategy options, priced

Ordered by savings-per-risk. Each states what evidence keeps its teeth.

### 3.1 Build the harness optimised — **~30–50 s, LOW risk** ⭐

`Cargo.toml` raises `opt-level` for `toyos-cc` and `toyos-ld` only. The test
harness, `toyos_build`, and their deps (`fatfs`, `gpt`, `image`, `bcachefs`)
build at **opt-level 0**, and two hot host loops live there.

Isolated benchmark of the screendump path — `Ppm::parse` plus `console_rows`
over a 1920×1080 frame, the exact loops from `tests/common/screen.rs`, compiled
standalone at both levels:

```
opt-level 0: 96.3 ms per screendump decode
opt-level 2:  2.6 ms per screendump decode      37x
```

Every poll of `screendump_while` parses 2.07 M pixels into a `Vec<[u8;3]>` and
hashes 16,080 glyph cells of 128 bytes each. `screen_console_scroll` polls at
250 ms across three rounds over ~99 s — **roughly 250 polls (derived from the
interval and the test's measured duration), so ~23 s of its 103 s is host-side
glyph decoding that opt-level 2 would return.** Twelve more screen boots poll
the same way, and `screendump_until` polls at 100 ms, where a 96 ms decode
nearly doubles the period.

Image assembly gains too (52 ms → 4 ms, §1.4), though it is small.

- **Saving: ~30–50 s (5–8%).** The range is honest: the decode ratio is
  measured, the poll count is derived.
- **Risk: LOW.** No test semantics change. Two things to check: the harness must
  keep `debug = true` so LLDB still works on it, and the screen family must be
  re-run green, because faster polling changes the *cadence* at which
  `screen_console_scroll` samples the panel. More samples is more coverage, not
  less — but it is a change to the observation, so it gets a green run before it
  lands.
- **Teeth:** none touched. Same assertions, same dumps, decoded faster.
- **Decision: orchestrator.**

### 3.2 Cache the image build per `(config, kernel_features)` — **~57 s, LOW–MEDIUM risk** ⭐

31 distinct image variants serve 72 boots (§1.4). Cache within a run.

- **Saving: 41 × 1.4 s = 57 s (9.5%).** (derived from measured components)
- **Risk: LOW–MEDIUM, and it has one sharp edge.** `create_boot_image`
  (`src/image.rs:51`) mints a fresh `Uuid::new_v4()` per call and writes it into
  both the GPT entry and `\toyos\log.guid`. Caching the *assembled image* would
  freeze that GUID across boots. **Cache the `(kernel_bytes, bootloader_bytes,
  initrd_bytes)` triple instead and call `create_boot_image` fresh every boot** —
  assembly is 52 ms, so there is nothing to save there anyway, and every boot
  keeps its own GUID.
- **Teeth:** the cache key must hash everything `build_test_image` consumes —
  config path, `kernel_features`, and the `extra_files` vector — and live in the
  test process, never on disk across runs. A stale key would boot the wrong
  image, which is exactly the class of defect this suite exists to catch, so the
  key is the review surface. `boot_partition_identity`'s premise (a fresh GUID
  per boot) is the negative control that would go red if the GUID froze; it
  stays in the suite unchanged and is the evidence this change kept its teeth.
- **Decision: orchestrator.**

### 3.3 Parallel boots with a serial timing-sensitive tail — **~600 s → ~175 s, HIGH risk** ⭐

QEMU machines are independent. The boot-dominated share is machine 407 s +
screen 166 s = 573 s.

Modelled with §3.1 and §3.2 already applied (boot share ≈ 488 s), audio serial,
shared block serial: (derived)

| width | parallel share | + audio | + shared | total | vs 600 s |
|---:|---:|---:|---:|---:|---:|
| N=1 | 488 | 34 | 17 | 539 | 1.1× |
| N=4 | 122 | 34 | 17 | **173** | **3.5×** |
| N=8 | 75 (floor) | 34 | 17 | 126 | 4.8× |

**Past N≈5 the critical path is one test.** `screen_console_scroll` at 103 s
(75 s after §3.1) becomes the floor, so §3.1 and §3.3 compound — cutting the
longest test is worth more once the rest is parallel than it is now. **Since
done: it is 27 s (§5.2), so the floor past N≈5 is the aggregate again and not
one name.**

**Host budget.** 14 cores. Each QEMU runs `smp: 2` by default, so ~3 host
threads. **N=4 ≈ 12 threads is the honest ceiling for one suite on this host**,
and that is before any cargo build.

- **Risk: HIGH, and it is entirely about whether the (d) classification stays
  trustworthy as tests are added.** The classification in §2 is a snapshot; the
  house adds gates with every fix, and a timing-sensitive test added next week
  that inherits "parallel" silently mismeasures. **The only safe shape is a
  default, not an opt-in: a test is serial-tail unless it declares itself
  parallel-safe**, so the failure mode of forgetting is a slow suite rather than
  a wrong one. That is the inverse of the usual ergonomic instinct and it is the
  whole safety argument.
- **Risk: gate A cannot run in the parallel phase at all** (§2d). It must run in
  the serial tail *and* with the parallel phase already drained — not merely
  "serial", but "alone".
- **Teeth:** every assertion runs unchanged; only the scheduling changes. The
  evidence that it kept its teeth is that the (d) set is enforced by the type
  system or by a default, not by a comment — a `ParallelSafe` marker a new test
  must opt into, checked at registration.
- **Decision: orchestrator for the mechanism; owner for the gate A consequence**
  (see §4).

### 3.4 Boot consolidation — **~44 s, MEDIUM risk**

The eight groups in §2b.

- **Saving: 12 boots × 3.7 s = 44 s (7%).** (derived)
- **Risk: MEDIUM.** Three specific hazards: the capture race (task #84); a test
  that panics or halts the guest ends the boot for everything queued behind it
  (`double_fault_stack`, the panic screen tests, `nvme_wide_sector`,
  `va_exhaustion`); and cross-test interference, which is already why
  `readdir_bound` has its own boot — it fills `/tmp` to the VFS listing limit and
  would refuse every later `read_dir` in that guest.
- **Recommendation: take only the two groups that are pure observers of one
  boot's log** — `metalcase/Metal` (4 → 1) and `testcases/Metal/i8042-trace`
  (3 → 1). Those six tests read different parts of the same boot's output and
  none of them kills the guest. That is 5 boots, ~19 s, at genuinely low risk.
  The rest is not worth its risk against §3.1–3.3.
- **Teeth:** each consolidated test keeps every assertion it has; the review
  question is whether any of them *writes* state a later one reads. If the
  answer is not obviously no, it does not get consolidated.
- **Decision: orchestrator.**

### 3.5 Kernel-variant parallel pre-pass — **rejected, disk-blocked**

Building all 31 kernel variants up front, in parallel, would turn 124–155 s of
serial cold rebuild into ~45–60 s of wall time on 14 cores. It does not work:
parallel cargo invocations against one `kernel/target` serialise on cargo's own
target lock, so it needs 31 separate target dirs, and `kernel/target` measures
**7.4 GB**. There is 111 GB free. **Not viable.** Recorded so nobody re-derives
it.

### 3.6 One runtime-actuator kernel instead of 31 feature kernels — **owner's call, not recommended**

Replacing the 31 compile-time feature sets with one kernel carrying every
actuator behind `SYS_DEBUG` actions would cut the cold-rebuild cost (§1.5) from
124–155 s to near zero.

**It is a coverage trade, not a cost optimisation, which is why it is the
owner's.** Today the binary under test is the shipping kernel plus one switch.
Several of these features do not merely inject a value — `xhci-deaf-controller`,
`i8042-fadt-denial`, `test-tiny-va`, `test-heap-ceiling`, `usb-flush-fails`
change which path runs, and some exist precisely to make the shipping path fail.
A runtime-selected actuator kernel is a *different binary* with dead branches
the shipping one does not have, and CLAUDE.md's rule that "a feature that merely
re-states what the code does is not an actuator" cuts against collapsing them.

Presented, priced, **not recommended.**

### 3.7 Selective test running — **owner's call, and the audit's position is no**

The house rule is that every worker runs everything. Changing it trades
confidence for speed.

**What it would have missed, from this tree's own evidence.**
`specs/metal-track-history.md` records ~70 defects found in code whose own suites
were green, and states the reusable form: mutating your implementation tests the
paths you wrote, never the states you did not think to construct. The failure
counts mined from this session's transcripts say the same thing from the other
end — the tests that go red most often are not the ones nearest the change:
`cache_eviction` 35 times, `i8042_health` 21, `boot_partition_identity` 20,
`screen_early_panic` 27. A change-proximity heuristic routes around exactly
those.

**The stronger argument is that it is unnecessary.** §3.1 + §3.2 + §3.3 take
600 s to ~175 s without touching what runs. Selective running should not be
spent before those are exhausted, because once adopted it is very hard to
un-adopt: every later red becomes "was that in scope?".

**Position: do not adopt. Owner's decision to overrule.**

### 3.8 The flake tax — **the largest single lever nobody has priced**

44 distinct full-suite runs appear in this session's transcripts:

| day | runs | suite-hours | median | red |
|---|---:|---:|---:|---:|
| 2026-08-01 | 8 | 2.65 | 1010.7 s | 5 |
| 2026-08-02 | 11 | 1.51 | 456.4 s | 4 |
| 2026-08-03 | 25 | 3.80 | 561.9 s | 17 |
| **total** | **44** | **7.96** | **553.3 s** | **26 (59%)** |

**26 of 44 full runs ended red, and 18 of those 26 failed on one or two tests.**
The most-failing names across every transcript in this session:

```
42  audio_tone_load (smp=1)      21  i8042_health
35  cache_eviction               20  boot_partition_identity
32  audio_tone_load (smp=8)      15  screen_fatal_halt
27  screen_early_panic           14  metal_sim_input
24  audio_tone (smp=8)           13  i8042_keyboard
```

**Audio is 108 of the top-ten failure count.** Known-issues already records that
gate A can fail a run on `drains` alone, with no gap and no underrun — a per-run
failure that carries no evidence of harm. **This audit's contribution is the
measurement: that defect is the single largest source of red full runs in the
tree, and each one costs an agent a ~600 s re-run to clear.** At ~15–25 runs a
day and a 59% red rate, that is on the order of an hour a day of re-running for
verdicts that do not mean anything.

Fixing gate A's per-run verdict is worth more than any scheduling change here.
It is a coverage question, so it is the **owner's**, and it is already open.

A `--rerun-failed` mode is the tempting mechanical fix and should be treated
carefully: it shortens *diagnosis* usefully, but if a rerun-only-failures green
is allowed to count as a green, that is §3.7 by the back door. **Whether it may
close a claim is the owner's call; building it as a diagnosis aid is not.**

---

## 4. The multiplier: N workers × M runs a day

**Measured, 2026-08-03: 25 full-suite runs, 3.80 hours of suite time in one
day**, across the agents working in this tree. Aug 1 and Aug 2 were 8 and 11
runs. Call it 15–25 runs a day and rising with the agent count.

At the 2026-08-03 rate:

| scenario | per run | ×25 runs/day | saved/day |
|---|---:|---:|---:|
| today | ~600 s | 4.2 h | — |
| §3.1 + §3.2 (no scheduling change) | ~515 s | 3.6 h | 0.6 h |
| + §3.4 (two safe groups) | ~495 s | 3.4 h | 0.8 h |
| + §3.3 at N=4 | ~175 s | 1.2 h | **3.0 h** |
| + gate A verdict fixed (§3.8) | ~175 s, 59% → ~20% red | ~0.9 h | **3.3 h** |

Two things this table hides, both important.

**Those hours overlap.** 25 runs did not take 3.8 hours of wall clock — several
agents ran concurrently, which is why individual runs stretched from 437 s to
752 s for near-identical test counts. **The saving is not just time, it is the
contention that inflates everyone else's runs.** Cutting each run by 3.4× cuts
the window in which any two runs collide by more than 3.4×.

**And the core budget is global, not per-suite.** This is the crux for the
worktree direction.

### 4.1 Per-worktree parallel suites (task #117) — three hard constraints

Priced as a live scenario, not a hypothetical.

**Constraint 1 — the rustup link is a single global symlink, and worktrees
would fight over it.** `~/.rustup/toolchains/toyos` is one link:

```
toyos -> /Users/jan/Dev/jan/toyos/rust/build/aarch64-apple-darwin/stage2
```

`src/toolchain.rs`'s `relink_needed` returns true whenever that link does not
point at *the current tree's* stage2, and `ensure` then re-links it. **Two
worktrees would relink it away from each other on every build**, and a build in
tree A between B's relink and A's next check either uses B's compiler or fails.
That failure already has a name in known-issues §6 — `'rustc' is not installed
for the custom toolchain 'toyos'` — and today it is a within-tree race window.
Worktrees would make it the steady state. **This is a hard blocker for #117 and
it must be solved before any worktree runs a build**, not discovered by it.

**Constraint 2 — disk.** Measured:

```
rust/build              47 G
kernel/target          7.4 G
target                 6.5 G
userland/target        1.6 G
tests/…/target         366 M
whole tree              77 G      free: 111 G
```

**One additional full worktree fits; two do not.** Sharing `rust/build` is the
only way past that, and it is the same object Constraint 1 is about.

**Constraint 3 — cores.** 14. One suite at N=1 uses ~3 host threads; at N=4,
~12. **Intra-suite parallelism (§3.3) and inter-worktree parallelism are the
same lever spent twice.** Four agents each running a 4-wide suite is 16 QEMUs on
14 cores, which is slower than serial and mismeasures everything. If both
directions are wanted, the parallel width has to be a **global semaphore** — a
lock outside any one tree, in the spirit of `src/buildlock.rs` but shared across
worktrees, that hands out QEMU slots against a fixed budget.

**Constraint 4, and it is the owner's — gate A stops being certifiable.**
`tests/audio-baseline.toml`'s numbers were recorded with one QEMU at a time and
no concurrent agents for the whole session. With worktrees that condition is
never met again. Either:

- gate A's fast tier leaves `cargo test` for a quiet-host job (and every worker
  stops certifying audio on every run — a real reduction in when the gate
  fires), or
- its per-run ceilings are re-derived under load (which widens them, weakening
  the gate), or
- the global semaphore reserves the whole host whenever any suite reaches its
  audio block (which serialises every agent behind every other agent's audio
  block — priced at 34 s each, ~14 minutes a day at 25 runs).

**All three are worse than today in some direction. This is the owner's
decision and it should be made before #117 lands, not after.**

---

## 5. Recommendation, ordered by savings-per-risk

| # | change | saving | risk | decision |
|---:|---|---:|---|---|
| 1 | §3.1 harness at opt-level 2 | 30–50 s | LOW | orchestrator |
| 2 | §3.2 cache the build triple per `(config, features)` | 57 s | LOW–MED | orchestrator |
| 3 | §3.4 consolidate the two observer-only groups | ~19 s | LOW | orchestrator |
| 4 | §3.3 parallel boots, N=4, serial-by-default | ~320 s | HIGH | orchestrator (mechanism) |
| 5 | §3.8 gate A's per-run verdict | ~1 h/day of re-runs | coverage | **owner** |
| 6 | §4.1 worktree blockers 1–3 | enables #117 | HIGH | orchestrator |
| 7 | §4.1 constraint 4, gate A under load | — | coverage | **owner** |
| 8 | §3.6 runtime-actuator kernel | 124–155 s on kernel-change runs | coverage | **owner**, not recommended |
| 9 | §3.7 selective running | large | confidence | **owner**, not recommended |

Waves 1–3 are independent of each other and of everything else, total ~130 s
(22%), and carry no coverage risk. **They should go first and they do not need
the classification in §2d to be right.** Wave 4 does, and its safety rests
entirely on the serial-by-default rule.

Items 5 and 7 are the two the owner has to rule on, and 7 blocks #117.

## 5.1 What waves 1–3 actually bought

Landed 2026-08-03 as `2a3b40b`, `da3d333` and `9801149`. Full suite green after,
233/233 in 605.7 s. Every figure below is a same-session A/B on one family, the
two arms alternated against an otherwise identical tree, because the two
full-run numbers either side of this work were taken in different contention
regimes and the difference between them is not attributable to it.

| wave | projected | measured | where |
|---|---:|---:|---|
| §3.1 opt-level 2 | 30–50 s | **−24.5 s** | screen family, 181.3 → 156.8 s |
| §3.2 memoize the parts | 57 s | **−3.2 s quiet, −31.3 s loaded** | i8042 family, 11–13 boots |
| §3.4 two groups | ~19 s | **−17.7 s** | i8042 family, 104.7 → 87.0 s |

**§3.1 landed inside its range and for a different reason than §3.1 gives.**
The decoder itself is 10× faster — `screen_decoder`, which is the decode alone
against a bitmap it renders, goes 41 ms → 4 ms. But `screen_console_scroll`, on
which the whole 30–50 s estimate rests, gains 5 s of its 110 rather than the
~23 s derived there: its poll loop ends on a marker the guest has to reach, so a
faster decode buys *samples*, not seconds. The saving is real and it is spread
across the other twelve screen tests, one to three seconds each, where
`screendump_until` polls at 100 ms and a 96 ms decode nearly doubled the period.

**§3.2's saving is a function of host load, and its 57 s is the loaded
regime.** What a hit removes is three `cargo` process spawns and an initrd
assembly. Under contention — the conditions every archived number in this
document was taken under — that is 2.4 s a boot, and one A/B/A measured
133.3 / 104.7 / 138.7 s on twelve tests. On a quiet warm host the same three
spawns are nearly free, and three consecutive A/B pairs measured 85.1/82.2,
86.8/83.5, 84.9/81.5 — 0.29 s a boot. Both are true. Since the tree runs 15–25
suites a day across several agents, the loaded regime is the usual one, but
§3.2's 57 s should be read as an upper figure rather than a constant.

Its memory cost, which §3.2 does not price: +48 MB of peak RSS on the i8042
family (2.199 → 2.247 GB), because a family shares one initrd and the memo holds
one copy of what each boot was building transiently anyway. Whole-image caching
would have held one ~200 MB initrd per kernel feature set; per-part keying is
what avoids that.

**§3.4's two groups are not "pure observers of one boot's log", and running
them found two things this document could not have.** A console is a stream and
`drain_serial` consumes it, so the first member to wait for a line the
compositor prints once takes it from every later member; the group now holds the
console. And **only one QEMU may be up at a time in this process** — every
instance shares one QMP socket path and one `test-bootable.img` under the pid's
temp dir, so a guest still up when the next test booted took that boot's socket
and it died before its first line. **That is the first thing §3.3 has to fix**,
before any two boots can overlap.

## 5.2 The longest test, and where its time actually was

`screen_console_scroll` — §1.2's number one at 103 s, and §3.3's floor past
N≈5 — is **27 s** as of this section. Same-session A/B on the tree that landed,
arms alternated, four runs: **110.5 / 111.4 s before, 27.4 / 27.5 s after** —
4.05×, and tight enough in both arms to read as one number each.

**§3.1 predicted the wrong cause and §5.1 already half-said so.** The whole
30–50 s estimate rested on host-side glyph decoding; opt-level 2 returned 5 s of
it. Instrumenting the poll loop says why. Of 116.4 s in one run:

| | seconds |
|---|---:|
| boot + image build | 5.1 |
| host: 408 screendumps (QMP + read + `Ppm::parse`) | 4.5 |
| host: `console_rows` decode, 408 dumps | 0.7 |
| host: typing three commands over QMP, pgup/pgdn sleeps | 3.1 |
| **guest: `Console::write_bytes`** | **101.2** |

The guest number is measured inside the console, not inferred: a timer around
`write_bytes` and around the stdout mirror beside it, printed when the batch
carrying `CHURN-DONE` arrived. **The mirror — 469 KB through the kernel log ring,
the 16550 and `log_file` — cost 0.02 s of the 101.** Splitting `write_bytes`
further puts ~98% of it in one place: the `draw_char` loop in `flush`, which
recomposes essentially the whole panel because a scroll of any depth changes
every row.

So the cost is **one full-panel recompose per console read**, and the console
reads its pipe 4096 bytes at a time. Across the three rounds that came out at a
flat **0.213 ms per byte of output** (0.213 / 0.254 / 0.212 for rounds 1–3),
which is the whole cost model: this test cost what it printed.

**What the workload was resized on.** 1750 lines, 458 KiB. The body width was
`5 + 37i mod 500` and the three rounds all started at line 0, so rounds 1 and 2
were strict *prefixes* of round 3 and the run walked 3.5 periods of a
500-period sequence. It is now a sweep of the panel's width plus a whole number
of extra panels — `5 + 37i mod cols + cols * WRAPS[i mod 8]` — walked by three
rounds over **disjoint** stretches of 100/60/100. Two properties replace the
line count:

- 37 is coprime with the 240-column panel, so any 240 consecutive lines end in
  every column exactly once. Column coverage is now a property of the
  construction rather than of how long the run is.
- `WRAPS = [0,1,0,2,0,1,0,0]` puts a one-row, a two-row and a three-row line in
  every eight, so a short run cannot lose the soft-wrap path.

**The second one is there because the first attempt lost it.** A plain sweep of
two panel widths keeps the coprime property, and 260 lines of it measured
240/240 columns — and **zero** lines wrapping twice, against 126 before. A
partial period of `37i mod 480` covers a structured subset (its values are
`37m + k` for `m < 13`), so it never reaches its own top; the lines that occupy
three rows all live at indices the run does not visit. Checked on the shipped
parameters before shipping them, not inferred.

The workload that landed is 260 lines and 65.5 KiB, and it is *denser* than what
it replaces on the thing that matters: **107 lines shorter than the line before
them, against 128 in 458 KiB** — the transition that leaves cells past the end,
at six times the events per byte. 7.0× the bytes removed; 4.5× the wall clock,
because boot and the QMP typing do not shrink with it.

**Teeth, measured rather than argued.** Deliberate re-breaks of the console, each
red on the reduced workload, all inside round 1 — within the first 100 lines of
260, where the old workload's first round was 400:

| break | red in |
|---|---:|
| a cell recorded painted that was never blitted | 10.2 s |
| the same, confined to rows the marker never lands on | 10.8 s |
| `scroll` leaves the old bottom row in place | 12.4 s |

(A fourth, `damage` returning a span one column short, was red in 9 s against an
earlier iteration of this workload and was not re-run against the final one.)

The second exists because the others corrupt the marker row too, so they fire the
marker check rather than the row comparison. Confining the break to rows the
marker never lands on makes the character-for-character comparison itself the
thing that fails, and it reports `panel row 0 … the row on screen is LONGER than
the line that belongs there` — the sentence the test was written to be able to
say.

**One structural change came out of watching those breaks.** The poll loop used
to ask the panel both "is the round over" and "is the panel right" at once, so a
broken panel could not satisfy it and spent the full 90 s timeout before
reporting that a marker never arrived. The console writes the glass before it
mirrors the same bytes to its own stdout, so the console *stream* answers the
first question and the panel is left to answer only the second: a break now
reports in 9–12 s with the offending row, and the churn is no longer sampled
408 times. Nothing observed was lost — those dumps were the loop's exit test and
were never asserted on.

**And one test was passing on nothing.** `test_screen_churn` is a workload with
no verdict, but it was in the shared boot's Rust registry, where it ran with
default arguments, printed 400 lines to a console nothing was reading, and
passed on its exit code. Making its arguments required is what surfaced that; it
is in `RUST_SKIP` now, beside the other bins that are driven rather than run.

Two things this found and did not fix are in `specs/known-issues.md` §8: the
collapsed-scroll paint the workload believed it exercised is unreachable through
a pipe and is asserted by nothing, and `Console::flush` records the rightmost
glyph column as painted when the scrollbar clamped the blit away from it.

The screen family is 14 tests, green, **72.7 s** with this in it.

## 5.3 What wave 4 actually bought, and the number it cannot reach

Landed 2026-08-03. The mechanism is §3.3's: per-boot images and per-worker
scratch (#120), the `build_toyos_bins` race closed (#121), a `Sched` on every
registration entry, a parallel phase, a serial tail, gate A alone.

**Every figure below is one session against one HEAD**, both arms green, 238
tests each, on the host §1's numbers were taken on. They were taken before
2026-08-03 20:22, which is when another worktree claimed the shared sysroot for
an unlanded ABI delta; every arm attempted after that is a build refusal rather
than a measurement and none is reported here.

| width | suite | vs serial |
|---:|---:|---:|
| `--jobs 1` | **569.6 s** | — |
| `--jobs 4` | **327.5 s** | **1.74×** |

§3.3 modelled N=4 at 173 s and 3.5×. **It is 327.5 s and 1.74×, and the whole of
the gap is that the model assumed a serial tail of nothing but audio.** The
measured phases, from the per-test durations each log prints:

| phase | tests | width 1 | width 4 |
|---|---:|---:|---:|
| parallel (sum of durations) | 69 | 428.3 s | 449.3 s |
| parallel (wall) | 69 | 428.3 s | ~190 s |
| serial tail | 165 | 103.1 s | 107.1 s |
| gate A | 4 | 30.0 s | 30.0 s |

Three findings, in the order they bite.

**The serial tail is 137 s and no width touches it.** 103 s of tail plus 30 s of
gate A is the floor of any run, so the two-minute target this wave was given is
**not reachable by scheduling** — an infinitely wide parallel phase still lands
near 140 s. What is in that 137 s is now a list rather than a feeling: the shared
block (~20 s), twelve machine tests with a wall-clock margin (88 s), and gate A's
four boots (30 s). Both remaining levers are the owner's, because both are
coverage: gate A's four boots must be alone (§2d), and each tail entry is a test
whose assertion is a margin. Nothing here should be moved to shorten a suite.

**The parallel phase's floor was one test, exactly as §3.3 said, and it was a
harder floor than §3.3 thought.** `screen_console_scroll` measured 108 s, 110 s
and 116 s at widths 1, 4 and 8 — it barely moved, because what it waits on is the
guest reaching a marker rather than host work. At width 4 the phase's 449 s of
test time compressed to ~190 s of wall clock, a 2.4× on 4 workers, and the
missing 1.6× is workers idling behind that one job.

**Every number above is of the *pre-cut* `screen_console_scroll`.** §5.2 landed
in the same window and took that test from 110.5 s to 27.4 s. So this wave's
parallel-phase floor is already gone, the §3.3 claim that "past N≈5 the critical
path is one test" no longer holds, **and the width question has to be re-asked
against the new duration profile** — the deficit these numbers attribute to one
long job will redistribute, and the next longest parallel jobs measured here are
machine tests (`xhci_deaf_registers` ~48 s and `usb_flush_optional` ~39 s at
width 4). Nothing in this suite dispatches longest-first: the durations that
would order it are not in the tree, and adding a hand-maintained list of long
tests would be a second registration to keep true. That is the next lever and it
wants the post-cut profile, not this one.

**Two classifications in §2d's snapshot were wrong, and the suite said so.**

- `i8042_fadt_denial` is named in §2d for "compares two boots' guest `boot_millis`
  with a 300 ms margin (`tests/toyos.rs:4535`)". That comparison is
  `i8042_absent`'s and has been for some time; `i8042_fadt_denial` injects five
  keys and asserts they arrive. Both are in the tail — the first on its margin,
  the second because it is host-paced injection — but the *reason* matters for
  the next person re-deriving the set.
- **The shared block is timing-sensitive, and only running it wide showed it.**
  It was declared `Sched::Parallel` on the grounds that its 5 and 10 second
  ceilings are 50× a median under a tenth of a second. At width 4
  `allocator_stress` went from 1 s to past its 5 s. A per-test wall-clock ceiling
  *is* the tail's definition; the reasoning simply had the wrong median in it.
  It cost 114 red of 238 to learn, because a timeout desynchronises every later
  test on that boot — known-issues §6, and a defect that predates this work.
- `xhci_second_controller` joins them on its own evidence: at width 4 its four
  injected pointer events arrived and **all five keys were lost**, which reads
  exactly like the defect it exists to catch. Host-paced injection has no flow
  control, and a lost keystroke is indistinguishable from a driver that dropped
  one. The other injection tests were green at width 4 and 8 and stay parallel;
  this is the class to watch as the width rises.

**The harness's own margins are margins on the host, and had to be re-derived.**
`wait_for_ready`'s ten seconds were set when one guest had the machine; two boots
exceeded it during this work, one of them in a phase running a *single* guest,
because the tree runs 15–25 suites a day across several agents (§4). It is now
ten seconds per guest the phase may have up, never fewer than two guests' worth.
No test asserts on boot time by that clock — the two that assert on boot time
read the guest's own stamps and are both in the tail.

**Default width 4, and the honest reason it is not 8.** §4.1 constraint 3 gives
14 cores against ~3 host threads a guest; 8 wide is 24 threads on 14 cores, and
the one width-8 run this session that was not invalidated by an unrelated build
refusal came in at 264.8 s — faster, but on a tree whose shared block was still
in the parallel phase, so not comparable to the 327.5 s above. **The measured
case for 8 is not made**, and it should be made again from scratch against §5.2's
`screen_console_scroll`, since that is what decides how much of a wide phase is
real work rather than idle lanes. The case *against* is unchanged by either:
every failure this work found was a wall-clock margin closing under
contention, and 8 closes them further. The width is a flag; raise it deliberately
and read §4.1 constraint 3 first.

**What it does not preclude.** The width is a plain number handed to one work
queue, so the global slot budget §4.1 constraint 3 asks for — a semaphore across
worktrees rather than within one suite — has one place to go.

**Ergonomic cost, unpriced by §3.3.** A test's evidence lines (`[i8042] …`,
`[usb] …`) go straight to stderr, so at width 4 they interleave and lose their
association with the test that printed them. The PASS/FAIL lines carry their own
names and are unaffected. `--nocapture` prefixes each serial line with its boot's
number for the same reason.

## 5.4 Wave 5: the two-minute suite

The target was the owner's: the steady-state suite under 120 s. §5.3 had said it
was **not reachable by scheduling**, because 103 s of serial tail plus 30 s of
gate A is a floor no width touches. That was true of the tail as it stood and
false about the tail: **twelve of its sixteen entries were there for a reason
that does not survive being read.** What follows is what each lever bought, then
the three ledgers.

### 5.4.1 Where the tail actually came from

The tail was 167.3 s: the shared block plus sixteen machine tests. Sorting them
by *what the assertion is* rather than by what the registration said:

- **Four are host-clock claims and stay.** `xhci_flap`'s two QMP writes have to
  land inside one 100 ms debounce or the state under test never happens;
  `i8042_absent` compares two boots' `Boot: complete` with a 300 ms allowance;
  `metal_sim_null_audio` puts an 8 s ceiling on a 3.3 s host-measured drain;
  `xhci_second_controller` lost all five of its keys at width 4 when the phase
  first landed (§5.3).
- **The shared block was never a host-clock claim at all.** Its 5 and 10 second
  ceilings are `run_test` timeouts — liveness guards on a guest that might
  wedge — and the verdict in every one of its 153 tests is an exit code and an
  expected stdout. [`qemu::budget`] now multiplies every such ceiling by the
  phase's width, which is what `wait_for_ready` has done to the boot timeout
  since the phase existed.
- **Eleven inherited the tail from a neighbour or from a sentence about
  keystrokes.** `late_storage_connect` is "`xhci_slow_connect`'s shape against
  the disk's port" and makes no claim about when anything happened;
  `xhci_slow_connect` itself reads *both* its instants off the guest's boot
  clock and its own comment records six runs, three of them with four concurrent
  test processes on the machine, at 13 ms of worst excursion against 150 of
  slack. The five locale tests and `i8042_fadt_denial` type at the guest and
  wait on `serial_until` against a marker: what drops a keystroke there is the
  PS/2 controller's sixteen-byte queue, not the host's load, and none of them
  puts more than a handful in flight before waiting on what came back —
  `i8042_kbd_echo` has run that shape at width 4 since the phase landed.

### 5.4.2 What each lever bought

| lever | where | effect |
|---|---|---|
| `drain_until` instead of `drain_serial(20 s)` | `double_fault_stack` | **32 s → 3 s** |
| `qemu::budget`, shared block to the phase | tail | −13 s from the tail |
| ten conversions | tail | 167.3 s → 37.4 s, and six tests to seven |
| longest-job-first on a measured profile | parallel phase | see §5.4.5 |
| inert-actuator folding | image builds | 5 kernel variants → 1 |

**Where it landed. 246/246 green in 109.1 s**, at the default width of 12
(§5.4.7), on a quiet host — `uptime` load 5.10 falling to 3.44, no other
worktree building or booting:

| | seconds |
|---|---:|
| parallel phase, 239 tests in 65 tasks | **42.1** |
| serial tail, 7 tests | **37.4** |
| gate A, 4 boots | **~30** |
| **suite** | **109.1** |

Same session, same tree, alternated: **125.6 s at width 8**, and 318.4 s at
width 12 on the *first* run after main's kernel changes arrived — §1.5's cold
variant tax, about 180 s of it, paid once by whoever runs first and by nothing
in this document. Two earlier runs of the 240-test tree came in at **107.3 s and
118.2 s** with other worktrees busy throughout, so the figure survives the load
the tree actually runs at as well as the absence of it.

Against the starting point: **327.5 s at width 4** before this wave and before
`screen_console_scroll`'s cut, and **433.8 s** measured on this branch's base
under five concurrent suites.

**Gate A is 28% of a green run and the serial tail is another 34%.** Two thirds
of the suite is now the part that cannot share the host, which is where the next
lever has to come from and both halves of it are the owner's.

One earlier run is worth keeping for what it shows about gate A's variance: the
same suite came in at **139.7 s** with gate A at **46.5 s** — 30 s plus two
confirmations, because two of its four configs saw a dropout and the fast tier
re-booted each once to check it reproduced. Neither did. A gate A that has to
confirm costs half again as much as one that does not, and nothing schedules
around that.

**`double_fault_stack` is the shape to look for elsewhere.** A guest the fatal
path has halted does not *exit*: every CPU is stopped and QEMU stays up, so
`drain_serial`'s ceiling has nothing to disconnect it and the drain waits the
whole twenty seconds for a machine that will never speak again. Every other
20 s drain in the harness follows `run shutdown`, where QEMU quits and the
reader disconnects, and costs nothing. One test in thirteen had the halting
shape and it was the suite's second-most expensive.

### 5.4.3 Ledger 1 — kernel feature inertness

The question is not what a feature is *for*; it is whether a kernel carrying it
boots, schedules, drives its devices and panics identically to one that does
not. **Inert** means every `#[cfg]` site is inside a `SYS_DEBUG` action arm that
nothing but a test can reach. Everything else keeps its own build.

| feature | class | why |
|---|---|---|
| `test-fatal-halt` | **inert** | one `SYS_DEBUG` arm + one const |
| `test-screen-graffiti` | **inert** | one arm + `panic_console::graffiti`, called from it alone |
| `test-double-fault` | **inert** | one arm |
| `test-heap-ceiling` | **inert** | three arms + `debug_heap_alloc`, called from them alone |
| `debug-wait` | inert in practice | a boot-time spin the harness only sets under `--debug`; never in the suite |
| `test-early-panic`, `test-late-panic` | perturbing | the boot panics |
| `test-input-merge` | perturbing | scripts the input core at end of boot |
| `test-tiny-va` | perturbing | moves `vma`'s arena |
| `test-small-caches` | perturbing | moves both disk-cache ceilings |
| `log-rotate-fast` | perturbing | moves `MAX_LOG_BYTES` |
| `i8042-trace` | perturbing | a log line per drain |
| `i8042-fault`, `i8042-budget-expired`, `i8042-fadt-denial`, `i8042-kbd-echo`, `i8042-fast-health` | perturbing | each changes what the probe or the ISR does on every boot |
| `xhci-one-slot`, `xhci-deaf-controller`, `xhci-deaf-port`, `xhci-slow-connect`, `xhci-slow-storage-connect`, `xhci-portsc-rw1c`, `xhci-hid-break-*` | perturbing | each replaces a register or a completion on the live path |
| `xhci-xecp-selftest`, `xhci-descriptor-selftest` | perturbing | run a walk at init |
| `usb-storage-gate` | **conditionally inert** | acts only on a disk carrying the stamp, but scans the bus and logs on every boot, so a boot with it is not a boot without it |
| `usb-flush-unimplemented`, `usb-flush-fails`, `usb-transport-break` | perturbing | replace a verdict or abandon a transfer |
| `fat-backing-read-fails` | perturbing | every FAT page re-read fails |
| `iommu-context-absent`, `iommu-empty-domain` | perturbing | mis-program the unit at init |
| `sched-check` | perturbing | extra work in every scheduler pass. **No test uses it** |

**Four of thirty-one are inert**, and `kernel/Cargo.toml`'s `test-actuators`
unions them: five distinct kernel builds (plain plus four) become one. A test
still names the actuator it wants — that is what its assertion is about — and
`qemu::fold_inert` is the single place the name stops being a separate kernel.

**The owner's "collapse toward a handful" needs §3.6, and this is not it.** The
other twenty-seven each change a path a boot takes; unioning any two of them
gives a kernel where both subsystems are broken, and every one of these tests
boots a whole machine. Making them runtime-selected is §3.6 — a different binary
with dead branches the shipping one does not have — and stays the owner's call
and not recommended.

### 5.4.4 Ledger 2 — coverage

**Nothing was dropped.** Every assertion in the suite is the assertion it was,
and the test count did not move. What changed is who runs beside whom, and one
number:

| change | what it could have guarded | why it is acceptable |
|---|---|---|
| `run_test`/`wait_for_console`/`screendump_while` ceilings × width | a guest that wedges is reported later | none of them is a verdict: every test whose pass depends on a deadline is in the tail and reads the *guest's* clock. A generous liveness guard costs diagnosis time; a tight one under load costs a false red, which is the failure this suite has most of |
| `drain_until` on `double_fault_stack` | a line printed after `[ist1] used` | the kernel writes that line last, from `halt_all_cpus` after `panic_flush`; `DOUBLE FAULT` and the whole report precede it, and all four assertions read them |
| eleven Serial → Parallel | a test mismeasuring a machine it does not have to itself | each is justified in its registration comment, and the re-run-alone pass (§5.4.6) turns a wrong answer into a named red rather than a quiet pass |
| `test-actuators` folded into every test kernel | a test kernel carrying `SYS_DEBUG` arms it does not use | the arms are unreachable without the syscall; no test asserts that an unknown action is refused; and the shipping build (`cargo run`, `--diag-boot`) does not go through `fold_inert` at all |

Two things this wave found and left alone, both **needing the owner** and
neither landed:

- **Gate A's 30 s is 25% of the target and cannot be touched without him.**
  Four boots, alone, is the floor of any run that certifies audio.
- **`xhci_flap` is the one test whose red is genuinely ambiguous under load**,
  and its own message says so. It stays serial.

### 5.4.5 Longest job first

A phase's wall clock is `max(sum / width, longest job)`, and FIFO reaches the
first term only when no long job is dispatched late. Declaration order puts the
feature-carrying tests last — deliberately, to keep the kernel rebuilds
together — which is the worst order for a wide phase: `xhci_hid_break` and
`xhci_deaf_registers` are two of the three longest jobs in the suite and both
sit in the last quarter of `MACHINE_TESTS`.

The order is taken from **what the last run in this worktree measured**,
recorded under `target/` and merged rather than replaced so a filtered run does
not throw away a full one's profile. A hand-maintained list of long tests was
rejected for the reason §5.3 gives — a second registration to keep true — and a
name the file has never seen sorts *first*, so a new test is assumed long until
it has been timed once.

### 5.4.7 The width, re-asked against the post-cut profile

§5.3 left this open and said it had to be re-made from scratch. Same session,
same HEAD, other worktrees' suites draining from `uptime` load 32 to 6:

| width | parallel | tail | suite |
|---:|---:|---:|---:|
| 4 | 182.7 s | 30.5 s | 274.1 s |
| 8 | **91.7 s** | 32.1 s | 183.2 s |
| 12 | 126.0 s | 27.6 s | 187.2 s |

**That table says eight, and it is wrong about twelve.** It was taken while
`drain_serial` was still width-scaled, and at width 12 `metal_sim_pointer_churn`
— twenty-four paced drains — *was* the phase: 126.0 s of job inside a 126.0 s
wall. It is 18 s now. Re-run on the fixed tree, after main was merged, 240 tests
both, both green:

| width | parallel | tail | suite | host |
|---:|---:|---:|---:|---|
| 8 | 59.1 s | 34.0 s | **123.1 s** | load 7.6 → 8.3, no other guest |
| 12 | 43.8 s | 33.6 s | **107.3 s** | load 18.9 → 9.0 |

**Twelve, and the more loaded of the two runs is the faster one.** Eight packs
454.5 s of test time into 58.3 s — 7.8× on 8 workers, with its longest single
job at 32 s — so the phase is sum-bound rather than critical-path bound, and the
sum is what more workers divide. The earlier table's own numbers say the same
thing from the other side: 716 s of test time at width 8 and 713.7 s at width
12, so twelve workers did not make the work bigger.

**Twelve is the number for one suite, and the host has no budget.** Four agents
at twelve is 48 guests on 14 cores. The counting semaphore
`specs/worktrees.md` §6 asks for still does not exist, and until it does this is
a per-suite number with a machine-wide cost — which is the same warning §4.1
constraint 3 gave at four and is now three times louder.

Re-run once more after main's four landings, 246 tests, quiet host, alternated
in one session: **125.6 s at width 8 and 109.1 s at width 12**, both green, with
the parallel phase at 58.3 s and 42.1 s. That is the same 16 s and the same
direction as the pair above, on a tree six commits later and with six more
tests in it.

**Confidence: three clean runs at 12, two at 8**, across two trees. Nine further
attempts are not measurements and none is reported: seven died on `this worktree
and the shared sysroot disagree about toyos-abi/src` when another worktree
claimed the sysroot mid-run — the signature is a dozen or more identical
refusals, one per test that tried to build after the claim — one spent 84 s
queued behind the exclusive phase that followed, and one ran under five
concurrent suites.

### 5.4.6 Ledger 3 — recycling and contamination

**Every red the wide phase produces is re-run by itself**, after the phase has
drained, and both answers are findings:

- **red again** — the defect is real and the width had nothing to do with it.
- **green alone** — the test is red only when it shares the host, which makes
  its `Sched::Parallel` wrong. A bug in `tests/toyos.rs`, not in the kernel, and
  one nothing else in the suite can notice.

**A green retry does not turn the run green.** A rerun-only pass counting as a
pass is §3.7 by the back door; the failure line says which of the two it was and
the run stays red until the classification is fixed. That is the whole safety
argument for widening the phase: getting a scheduling answer wrong costs a red
run, never a quiet one.

A group member is re-run **as its group**, so the only thing that differs
between the two attempts is how many guests the host had.

**It found three things on its first three runs**, all under four other
worktrees' suites:

- **`ioapic_topology`: `Failed to read binary for toybox`, green alone.** Not a
  margin — a real race in the build system. `build_and_assemble` read
  `userland/target/x86_64-unknown-toyos/debug/<program>` straight out of cargo's
  output directory with nothing held, while another worker's config was
  relinking the same path. Cargo's own lock orders the two *builds* and says
  nothing about a read between them. Fixed by putting the build and the read
  under one `buildlock::artifact` hold, which is the fix `build_toyos_bins`
  already carries and for the same stated reason.
- **`i8042_mouse`: one discarded byte, green alone.** A pre-existing
  `Sched::Parallel`, red only under five concurrent suites.
- **`usb_transport_break`: "the transport broke 2 times; the injection is armed
  once per boot", green alone.** Also pre-existing, also five-suite load.

The last two are in `specs/known-issues.md`. Neither was introduced here and
neither reproduces on a host running one suite; both are exactly what the
mechanism exists to surface.

## 5.5 Wave 6: the regression, and pacing as the general fix

§5.4 certified 109.1 s at 246 tests. Two landings later the suite measured
**168.8 s at 248** — parallel 44.3 s (unchanged), gate A 29.7 s (unchanged),
serial tail **94.8 s against 37.4 s**. Both causes came in with the compositor
work at `c29859a` and both were individually defensible.

1. **`metal_sim_window_drag`, 35 s**, the single most expensive test in the
   suite. A good gate — a window drag must damage a bounded area — whose client
   ran a fixed 30 s. The host's whole sequence takes about 2.5 s of that.
2. **The `I8042_TRACE` group moved `Parallel` → `Serial`**, taking 22 s of tail
   with it. `i8042_mouse` fired 1000 `input-send-event` commands back to back
   and required 500 pointer events out of the guest, which is a drain rate.
   Group members share one `Sched`, so all three went.

### 5.5.1 What a fixed client deadline costs, and what replaces it

Five guest programs ended their run on a wall-clock duration because nothing in
the protocol let the host say "done" to a client it pokes through a mouse. Each
one is a test that pays the whole duration on every green run:

| client | was | ends on | test, before → after |
|---|---|---|---|
| `window_drag` | 30 s fixed | the host's second press | `metal_sim_window_drag` 35 s → 8 s |
| `i8042_mouse` | 10 s fixed | a right-button release | `i8042_mouse` 10 s → 0.55 s |
| `input_events` | 6 s fixed | a right-button release | `metal_sim_input` 3 s, `xhci_second_controller` 8 s → 3 s, `xhci_hid_break` 21 s → 10 s, `xhci_hotplug` 9 s → 6 s, `xhci_flap` 8 s → 6 s |

The marker is the right button, which no sequence driving any of these produces
for any other reason, and the run ends on its *release* so the pointer is left
with nothing held. Raising `input_events`'s deadline to a liveness ceiling
without giving its other three callers the marker cost 32 s, 36 s and 69 s on
one run — the failure mode of this design is loud and immediate.

`window_drag` has no marker: it ends on the press its host's sequence ends with,
and the host then waits for the compositor interval that closes after the client
has gone (`drain_until`, not a sleep).

### 5.5.2 Pacing: the general form of "this verdict is a rate"

`QemuInstance::run_test_hooked` injects a whole sequence in one call and holds
the serial reader while it does. The host therefore runs at its own speed, and
what reaches the guest is whatever survived the queues in between — so a packet
the guest was never given is indistinguishable from one it lost. That is the
whole defect behind two `Serial` registrations:

- `i8042_mouse` at width 12 delivered 127 and then 209 of its thousand. QEMU's
  PS/2 buffer silently drops a packet it has no room for.
- `xhci_second_controller` at width 4 delivered its four pointer events and lost
  all five keys.

`run_test_paced` is the same loop with the hook called on **every** console
line, so an injection can be driven by what the guest has printed.
`run_test_hooked` is written in terms of it. Two users:

- `i8042_mouse` keeps at most `MOUSE_LEAD` = 32 packets (96 bytes, against a
  256-byte ring in the kernel and QEMU's buffer above it) ahead of confirmed
  arrivals. The verdict moved from *at least half of the thousand arrived* to
  **all 1008 injected packets arrived**, plus `0 discarded`, `0 overruns`,
  `0 dropped`, `0 lost edges` off the driver's own line — counters that a
  starved guest could previously have explained and now cannot.
- `input_events_run` is the shared sequence of `metal_sim_input` and
  `xhci_second_controller` as a script whose every step waits for the guest to
  print what the step before it produced.

Both moved back to `Parallel`. A guest with less of the host is now a longer run
and never a smaller count.

`i8042_mouse` also reads the driver's periodic counter line, whose default
cadence is one per 10 s of typing — longer than the paced burst now takes. The
trace kernel gets `i8042-fast-health`, carried in `kernel/Cargo.toml` as an
implication of `i8042-trace` rather than asked for at the boot site: a kernel
build is keyed on the feature *set*, and asking for the pair took the parallel
phase from 44.3 s to 53.4 s for one extra kernel.

### 5.5.3 Ledger — coverage

**Nothing was dropped**, keeping §5.4.4's count at zero. Two assertions moved
and both got stronger:

| assertion | was | is |
|---|---|---|
| `i8042_mouse` delivery | `events.len() >= BURST / 2` | `events.len() == injected`, with every packet paced against an arrival |
| `i8042_mouse` counters | `0 discarded`, and only if a line happened to appear | `0 discarded`, `0 overruns`, `0 dropped`, `0 lost edges`, and the line is required |

One diagnosis changed shape. `metal_sim_window_drag` used to catch a title-bar
probe that landed in the content as a *third* press; the client now stops at
the second, so the same miss fails on the displacement instead. Staged: the
message is `carried it 0,41 — the press missed the title bar`.

Teeth, each observed red on the shortened tests and reverted:

| what was broken | what reported it |
|---|---|
| `TITLE_PROBE_PX` = −40, so the drag's press lands in the content | `the drag was supposed to carry the window 120,60 px and carried it 0,41` |
| `MSG_PRESENT` damaging the whole screen | `repainted 2073600 of 2073600 pixels in one frame` |
| `window_drag` counting no presses | `pressed inside its content 0 times, not twice` |
| `kernel/src/mouse.rs` dropping 1 pointer event in 400 | `timed out after 60s — 1002 of the 1004 packets injected came back out` |
| `DISCARDS` seeded to 1 | `the driver does not report `0 discarded`` |
| `kernel/src/keyboard.rs` dropping 1 key event in 3 | `typed "h", want it to contain "hello"` |

The fourth is the one that matters most: two lost packets in a thousand is a
green run under the old verdict and a named red under the new one.

### 5.5.4 The number

**108.0 s, 248 tests, all green** — parallel 43.8 s, serial tail 34.5 s, gate A
29.7 s. Same session, same host, against **168.8 s** measured on `c29859a`
before the change. The tail's seven remaining entries are the six §5.4 left
plus the drag test:

    metal_sim_window_drag 8s   metal_sim_null_audio 6s   netd_hostile_peer 4s
    i8042_absent 4s   xhci_slow_connect 3s   xhci_flap 6s
    late_storage_connect 3s

Confirmed at 112.5 s on a later run. Everything measured after that first pair
was taken on a host that turned out to be carrying **three other full suites and
a `toyos-sched-sim measure`** — `ps aux -r` and three `toyos-tests-<pid>`
directories in `$TMPDIR`, load 10.5 against the 1.2 the baseline was taken at.
Under that the same tree ran its parallel phase in 245.2 s. Two things came out
of it anyway, both fixes on their own terms and neither a response to the load:

- `shell_answers` typed ten times with a flat two seconds between, a
  twenty-second ceiling on a desktop coming up that does not scale with the
  phase. Now `qemu::budget(20 s)`.
- `usb_transport_break` is `Sched::Serial`. Its second `transport broke` line is
  the driver's recovery retrying against an endpoint still halted from the
  staged break, so "the recovery finished on its first try" was part of its
  verdict. Costs 3 s of tail.

One caution recorded rather than resolved: a run under that contention wedged in
the metal-sim desktop group — one vCPU at 100% for twenty minutes, no output,
killed rather than waited out (`metal_sim_compositor_stall`'s ceiling is
`budget(240 s)`, 48 minutes at width 12). That group is green three times out of
three in isolation on the same tree and green in every full run that completed.
Both are filed in `specs/known-issues.md`.

## 6. What this audit did not measure

- The split of a machine test's time *above* the 3.7 s floor into guest work
  versus host waiting. Per-test, that needs harness instrumentation.
- Whether opt-level 2 changes any screen test's outcome. §3.1 says it must be
  re-run; it was not re-run here. Since done: all 15 pass at both levels (§5.1).
- The 41 redundant image builds and the 12 consolidatable boots were attributed
  by static analysis of `BootOptions` sites. Six tests' shapes could not be
  resolved statically — `boot_partition_identity`, `readdir_bound`,
  `usb_flush_optional`, `xhci_hid_break`, and the two IOMMU tests added mid-audit
  — alongside the three that genuinely never boot. The counts are therefore
  **lower bounds** on redundancy, and exact on the shapes they name.
- The current test count. `cargo test -- --list` was re-run to confirm the
  post-growth census and the tree did not compile at that moment
  (`src/buildlock.rs`, another agent mid-edit). 229 is the last count this audit
  measured; ~231 is the source count now.
- Contention was present for every archived number. Nothing here should be
  A/B'd against these figures in a later session; re-measure against the same
  HEAD in one session, per CLAUDE.md.
