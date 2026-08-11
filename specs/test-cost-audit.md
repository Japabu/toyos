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

Counts here are the measurement's, not a running total: `screen_pager_keys`
(`testcases` / `Metal` / `test-late-panic`) has been added since and is a shape
of its own, so it joins no group above and removes nothing.

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

### 3.6 One runtime-actuator kernel instead of 45 feature kernels — **taken, 2026-08-10**

Replacing the compile-time feature sets with one kernel carrying every actuator
behind a boot parameter cuts the cold-rebuild cost (§1.5, §5.9.2) from 45 kernel
builds to two.

**It was presented as a coverage trade and not recommended**, on the grounds
that a runtime-selected actuator kernel is *a different binary with dead
branches the shipping one does not have*. **The owner took it on 2026-08-10, and
the split is what answers the objection**: the dead branches are in the test
kernel, which never ships, and without `boot-actuators` every accessor is
`const fn … { false }` — so the shipping kernel has no branch, no arm set and no
name in it, and `assert_no_actuators` refuses to write an image where it does.

That is strictly better than what it replaced. Before, 45 near-shipping kernels
each differed from the shipped binary in one *live* path and the shipped binary
was booted by one boot in the suite; now it is booted by every boot that does
not need a perturbation, which is most of them.

§5.9.7 is what was built, what it cost, and the one actuator that kept its own
build.

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

**Audio is 108 of the top-ten failure count.** It was already on record that
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
That failure already has a name in `specs/issues/build/` — `'rustc' is not installed
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

Two things this found and did not fix are in `specs/issues/hardware/`: the
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
  test on that boot — `specs/issues/build/`, and a defect that predates this work.
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
| `i8042-edge-race` | perturbing | widens one window in every scheduler pass |
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

**Two things above are false as of §5.9 and one of them was false when it was
written.** `test-heap-ceiling` is not inert — it also moves
`MAX_SYSINFO_THREADS` from 65,536 to 16, on a path `SYS_SYSINFO` runs — and
`fold_inert` put it on every kernel this suite booted, so no guest ever ran the
shipped bound. And folding *into every image* meant no test had ever booted the
kernel an image ships. Read §5.9 for the sweep that replaces this table.

**The whole of this table is superseded.** Its criterion — is a kernel carrying
the feature identical to one that does not — was the right question for a world
where a feature was a build. Since 2026-08-10 an actuator is a *boot parameter*
and the question does not arise: one test kernel carries all 47, arms whichever
the parameter names, and the shipping kernel carries none of them at all. Read
§5.9.7 for what replaced it, and §3.6 for the trade the owner took.

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
  once per boot", green alone.** Also pre-existing — and not load at all, which
  took until `specs/issues/hardware/` to establish. Five concurrent suites
  slowed the host enough for the guest to win a race it loses on a quiet one,
  which is the same variable KVM changes by 50x; the defect underneath was the
  driver's, and load was only ever the thing that exposed it.

Both are in `specs/issues/`. Neither was introduced here, and both are
exactly what the mechanism exists to surface.

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
  PS/2 device sums the motion it has no room to queue rather than queueing it,
  so the packets the host counted were never on the wire.
- `xhci_second_controller` at width 4 delivered its four pointer events and lost
  all five keys.

`run_test_paced` is the same loop with the hook called on **every** console
line, so an injection can be driven by what the guest has printed.
`run_test_hooked` is written in terms of it. Two users:

- `i8042_mouse` keeps at most `MOUSE_LEAD` = 4 packets (12 bytes, inside QEMU's
  16-byte `PS2_QUEUE_SIZE`, with a `const` assert saying so) ahead of confirmed
  arrivals. The verdict moved from *at least half of the thousand arrived* to
  **every injected packet arrived**, plus `0 discarded`, `0 overruns`,
  `0 dropped`, `0 lost edges` off the driver's own line — counters that a
  starved guest could previously have explained and now cannot. The lead was 32
  for its first year and that is what made the test flaky: `specs/issues/hardware/`.
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
- `usb_transport_break` is `Sched::Serial`. Costs 3 s of tail. **The reading of
  its second `transport broke` line recorded here — "the driver's recovery
  retrying against an endpoint still halted from the staged break" — was
  wrong.** The endpoint was Running; what stalled was the *device*, refusing the
  next command block because the Bulk-Only Reset had gone out while the
  abandoned transfer could still be answered. A driver defect that lost a write,
  not a measure of how much of the host the guest had
  (`specs/issues/hardware/`). The lesson for this document: a red attributed
  to load without the mechanism being read out of the log is a guess, and a
  costs table is where a guess stops being questioned.

One caution recorded rather than resolved: a run under that contention wedged in
the metal-sim desktop group — one vCPU at 100% for twenty minutes, no output,
killed rather than waited out (`metal_sim_compositor_stall`'s ceiling is
`budget(240 s)`, 48 minutes at width 12). That group is green three times out of
three in isolation on the same tree and green in every full run that completed.
Both are filed in `specs/issues/`.

## 5.6 The host's guest budget, and a run that knows it was invalid

Two things §4.1 and §5.4.7 left open, both landed 2026-08-05.

### 5.6.1 The counting semaphore §4.1 constraint 3 asked for

The width is a number for **one** suite. Nothing handed out the cores that two
suites both spend, and §5.4.7's own warning — "four agents at twelve is 48
guests on 14 cores" — was being paid daily. What it cost, from other agents'
2026-08-04 sessions: `screen_fatal_halt` red at 11 s under a second suite
against 3.3 s alone, `screen_console_panic` 39 s against 13 s, and about an hour
spent chasing a phantom regression that was three other `cargo test` processes
and a scheduler simulation at load 30.

`buildlock::guest_slot` is that semaphore. Twelve lock files in
`<common-dir>/toyos-build-locks/slots/`, and taking one is finding a file nobody
holds — `flock` rather than a pid file, so a slot a SIGKILLed suite had is free
the moment the process dies. Four properties are load-bearing:

- **The budget is 12**, the same number and the same measurement as the suite's
  own width (§5.4.7). One suite alone therefore gets exactly the machine it was
  measured on, and N suites divide it.
- **One slot per task, never per boot.** A worker holds at most one and never
  waits for a second while holding one, which is what makes it deadlock-free
  rather than lucky: several tests hold two guests at once, and a slot each
  would let twelve workers each hold one and each wait for another.
- **The wait is outside the task.** It lands in the phase's wall clock and in no
  test's duration, so a `PASS` time — and the profile `longest_first` orders
  on — stays a measurement of the test rather than of the queue.
- **It announces.** `[host-slots] waiting for a guest slot (…) — all 12 held by
  2 run(s): pid …` on the first failed scan and every 30 s after. An agent
  staring at silence kills and retries, which is the pathology `src/buildlock.rs`
  exists to remove.

`--host-slots N` overrides the budget and `0` turns it off, which is the only
way to measure a suite against one that has it.

**It has to be `cargo test --test toyos-build -- --host-slots N`.** A bare
`cargo test --` hands the flag to every test binary in the package, the `--lib`
unit tests among them, and libtest refuses an unknown flag by name: `error:
Unrecognized option: 'host-slots'`, exit 101, before the suite has started. The
form this section and CLAUDE.md carried was the bare one, and it never worked;
found 2026-08-07 by an A/B that failed in one second. `src/testargs.rs` cannot
catch it — that table governs the flags the *suite* reads, and this one never
reaches the suite.

Gate A takes one slot like everything else and reserves nothing. §4.1
constraint 4 offered a quiet-host reservation as one of three options; **the
owner ruled on 2026-08-04 that there are no measurement locks and no quiet-host
scheduling**, and the fast tier's verdict is harm rather than a ceiling, so the
question is closed and the mechanism is deliberately not built.

### 5.6.2 A run that spans a host suspend reports itself invalid

The dev machine is a laptop and the owner closes the lid; on 2026-08-04 that
killed three agents mid-run. CLAUDE.md already documented the signature —
bimodal timing, a tight cluster plus enormous outliers — and documented it as
something a *human* has to notice before recording a finding, which is to say
the harness never could.

It can, from the two clocks it already reads. `Instant` is `CLOCK_UPTIME_RAW`
here and `CLOCK_MONOTONIC` on Linux, neither of which advances while the machine
is asleep; `SystemTime` does. So `wall − monotonic` across an interval **is** the
suspended time, and no new source of truth is needed. It also explains why a
suspended run does not simply time out: every deadline in the harness is on the
monotonic clock, which stopped with the host, so the run comes back and reports
whatever the guest, QEMU's virtual clock and the guest's own millisecond stamps
made of the gap.

Every outcome carries the suspend that overlapped it, and one that crossed two
seconds or more is **neither a pass nor a fail**. `INVL` in the run's output, its
own list in the summary, and:

| exit | meaning |
|---:|---|
| 0 | every verdict was established |
| 1 | a real red — including a run that also had invalidated tests, because a red that survives is still a red |
| 2 | the run established nothing; the host stopped in the middle of it |

§5.6.3 adds no fourth status and deliberately: a declared red is not a red, and a
declaration the run found stale is one.

**Why 2 and not 0 or 1.** `--land`'s gate consumes this number, and 0 is a claim
that the tree passed — which a run measured across a stopped host did not
establish. Exiting 1 is no better: it sends an agent hunting a defect that is
not in the tree, which is the exact hour this exists to stop being spent. Cargo
propagates a test binary's own exit code (measured on this host with a
`harness = false` probe: `exit(2)` out of the binary is `cargo test` exit 2), so
`src/land.rs` reads it and says the gate did not finish a measurement, main was
untouched, and nothing is wrong with the branch.

The actuator is the clock source. A test cannot suspend the host it runs on —
the lid is what produces it and there is no API that asks for it — so
`clock::stage_suspend` moves the wall clock alone, which is precisely and only
what a suspend does to this pair; a staged reading is indistinguishable from a
real one for every consumer downstream, verdict and message alike. Two gates,
both host-side and booting nothing: `suspend_detector` for the detector, and
`suspend_invalidates_a_verdict` for what the suite does with it — a pass and a
fail across the jump are both `Invalid`, a pass and a fail across ordinary clock
jitter are unchanged, and those two rows are one line apart because **a suspend
that silently passes is as bad as one that silently fails**.

What it does not do: a suspend that lands *between* two tests invalidates
nothing, and is right not to. It is reported as a note, and the suite time
printed beside it is monotonic and already excludes it.

### 5.6.3 A known red is declared, not skipped

The blockage this closes: `--land`'s gate is the whole suite and refuses a
non-zero exit, so one test that is red for a reason everybody already knows
blocks every landing in the tree. The two ways out before this were both wrong.
Reclassifying `desktop_window_child` to `Sched::Serial` would have made the suite
green by destroying the only QEMU reproduction of #156. `--land --gate cargo
test -- --skip <name>` ran everything else, which was honest about *this*
landing and silent forever after: an exclusion typed on a command line cannot
notice that its defect is fixed, and lives in muscle memory rather than in the
tree. `--skip` is deleted; `EXPECTED_FAILURES` in `tests/toyos.rs` replaces it.

An entry names the test, the task, where the defect is written up, and the
failure messages the exemption covers. The verdict table gains two rows, and
`Verdict::Pass`/`Fail` each gain the entry that was consulted and *did not
apply* — so no arm can treat a case as covered by forgetting to look one up.
Exit status is unchanged: an expected failure is not a failure and never reaches
`Tally::exit_code`, so a run whose only reds were declared is exit 0 and
`src/land.rs` needs no change at all. The result line says `ok, NOT clean` and
names every entry that fired, because the whole hazard of the mechanism is a run
that reads as green to somebody who did not scroll up.

**The design question is what makes an entry stale, and it has two answers
rather than one.** The brief's requirement — *a listed test that passes is a red
run* — is what stops the list outliving the defect, and it is exact for a
failure that fires every time. It is wrong for one that does not:
`desktop_window_child` is intermittent (red alone and red wide, at a different
assertion each time), so a single green is one sample of a rate, and
`specs/audio-gate-history.md`'s standing lesson is that a verdict taken from one
sample is a verdict about nothing. Redding the run on it would fire on a healthy
tree and teach everybody to re-run until it went away — which is the exact habit
this mechanism exists to make unnecessary. So `Stale::OnAPass` is the strong
form and stays the default to reach for; `Stale::OnThisDate` is the honest
weaker one, and it does not claim to detect a fix. It claims that on a stated
day the entry reds and somebody looks. Both halves are the anti-rot property:
neither kind of entry can go quiet.

**What the message matcher does and does not buy.** `says` is a list of
alternatives quoted from the test's own messages, and a failure matching none of
them reds with a line saying the entry did not cover it. That pins *which*
assertion failed, not why, so a second defect reaching the same assertion is
absorbed — and where the real discriminator is a property of the log rather than
of the message, as #156's is (the guest stops emitting anything, the ~2 s
compositor stats line included), no cheap matcher reaches it. That one stays a
human's, which is why every `XFAIL` line prints the pointer to where it is
written up. Quotation rather than prose is deliberate: a restatement drifts
silently from what the test says, and a quotation reds the day somebody rewords
the assertion.

Three host-side gates, no guest, ~0.2 ms between them:
`expected_failure_verdicts` (the table, both directions),
`expected_failure_exit_status` (through `Tally`, to the exit code and the report
text), `expected_failure_entries` (the declaration's own shape, and the calendar
under `OnThisDate`). Eight deliberate breaks of the implementation, each red,
against the unbroken tree green — a listed pass filed as an ordinary pass, the
matcher absorbing everything, a stale or expired entry not redding, a review date
that never arrives, an expected failure filed as a pass, and the three
declaration checks each defeated in turn.

## 5.7 The host's build budget, and the count §5.6.1 was not

Added 2026-08-07, with ten worktrees on the host for the first time.

### 5.7.1 A worker that is compiling is not a guest

§5.6.1's four load-bearing properties are all still true, and one sentence in it
is what this section corrects: "one slot per task, never per boot". A task is not
a boot — it is *a build and then a boot*. `run_phase` takes the guest slot before
`run_task`, and the first thing `run_task` reaches is `build_boot_image`, which
compiles the kernel variant that boot needs. So twelve workers on a fresh profile
are twelve concurrent `cargo build`s and no guest at all, and the semaphore that
was written to stop four agents putting 48 guests on 14 cores was, in that
window, guarding nothing.

Measured on 2026-08-07 while the eight-landing storm in `specs/issues/`
was under way: **load average 49.9 on 14 cores, twelve `rustc`/`cargo` processes,
and exactly one QEMU guest live.** The one worker that had got as far as booting
had a fiftieth of the machine its wall-clock margins were written for. Two of the
suite's own liveness-margin tests (`blocked_dump`, then `screen_early_panic`)
went red in consecutive landing gates on a branch that touched only `tests/`,
each `ALONE … GREEN`.

The same hole is what "nothing bounds the build half of a landing gate" names: a
gate is `cargo test`, a `cargo test` is a build plus a suite, and only the suite
half was counted. It is not a separate defect and it does not need a separate
mechanism — a gate's builds *are* these builds.

### 5.7.2 The mechanism

`buildlock::build_slot` is a second counting semaphore over the same `slot()`
primitive `guest_slot` uses, in `<common-dir>/toyos-build-locks/build-slots/`.
Four properties, and three of them are inherited:

- **Its own directory, so its own count.** A single count for both would deadlock
  by construction: a suite holding all twelve guest slots would be waiting for a
  slot it is itself holding. Gate:
  `guests_and_builds_are_counted_separately`.
- **The budget is 4**, and unlike §5.6.1's twelve it is **policy rather than a
  measurement**. A build's own `-j` is already the whole machine, so the honest
  question is not how many builds fit but how far the machine may be
  oversubscribed; four builds against fourteen cores is about the ratio the
  measured pathology (twelve, at load 49.9) exceeded by three. A bound that is
  too generous is recoverable and one that is too tight makes every agent wait on
  every other, which is why the error is deliberately on the generous side.
  `--host-builds N` overrides it, `0` turns it off.
- **Below the memo, above every build lock.** `build_test_image` returns from its
  three-part memo before taking anything, so a boot that compiles nothing queues
  for nothing — which matters, since most of a run's ~76 boots are memo hits.
  Taken at `build::build`, `build::build_test_image`, `build::build_toyos_bins`
  and the libc archive in `tests/common/compile.rs`, which between them are every
  cargo invocation the build system makes.
- **The order is a constraint**: sysroot → host slot → build lock → artifact, at
  every acquirer of two of them. A build slot taken *before* the sysroot lock
  closes a cycle with a `--claim-sysroot`, which holds the sysroot and then wants
  a build slot; a build slot taken *inside* a build lock closes one with the
  escalation in `Held::act_if`. Both orders are stated in the module header
  because neither is checkable from a call site.

Gates in `src/buildlock.rs`, each watched red before and green after:
`a_full_host_makes_the_next_build_wait` (budget widened by one),
`a_killed_build_gives_its_slot_back` (the guard handed an fd that never locked),
`guests_and_builds_are_counted_separately` (one directory for both).

**What the first bounded run showed, and what it cannot show.** 287 tests,
700.3 s (456.3 s parallel, 212.9 s serial), on a host that was also carrying
another worktree's suite — one running `main`, and therefore *unbounded*. Peak
load 80.05. The bound bit: 146 `[host-builds]` lines, the longest single wait
49.3 s, and the message names its own process (`all 4 held by 1 holder(s): pid
10913`) because four workers of one suite is what filled it. So the mechanism
does what it says, and **the run establishes nothing about whether four is the
right number**: an A/B against `--host-builds 0` in that session would have
compared a bounded suite against an unbounded peer either way. The clean
measurement — one suite alone, four against off — is not taken here and wants a
quiet host. Until it is, the number is the policy above and the flag is how to
change it.

### 5.7.3 A queue is not silence

`flock` gives no progress and the exclusive holds here are minutes long. Eight
`--land` processes on the integration lock printed one `[build-lock] waiting …`
line each and then nothing, which is what a wedge looks like — and an agent that
kills a wedge puts its own gate back at the end of the queue. Every blocking
acquisition now repeats itself every 30 s, re-reading the holder each time so the
message follows the queue forward rather than naming whoever was in front when
the wait began. A thread does the talking and the kernel still keeps the queue,
so `flock`'s ordering is given up nowhere. Gate:
`a_lasting_wait_keeps_saying_so`, which is red on the tree as it stood.

## 5.8 Wave 7: the wall-clock verdict, and what was hiding behind it

**The direction CLAUDE.md does not state.** It says a red that is green alone
names the classification as the bug. The converse case is a machine-wide kernel
panic: it reds whichever test happened to be running, so the red's name is the
workload and never the cause — including when the harness re-runs that test
alone, it reds again, and the run reports the defect as real.

`specs/issues/build/`'s list of `Sched::Parallel` tests that red beside
other guests had one shared shape: a test waits a number of host seconds for the
guest to do something, and reports the *content* it was going to assert when the
number expires. §5.5.2 fixed that for injection by pacing; this is the same idea
for waiting.

### 5.8.1 What was changed

- **`await_guest`** (`tests/toyos.rs`) replaces `serial_until`'s span of
  seconds. It ends when the guest goes quiet (15 s of no console output) or
  wedges (300 s flat, unscaled — width is the correction for a *slow* guest, and
  a slow guest keeps talking). Sixteen call sites, including every wait in
  `desktop_audio_client`, both locale wizards, `blocked_dump`'s report and
  `screen_console_scroll`'s round marker.
- **The two compositor tails** — `metal_sim_compositor_stall` and
  `metal_sim_client_death` — asked for two frame batches *inside 20 s*. They now
  ask for two frame batches.
- **`shell_echoes` and `open_terminal`**'s retype loops became a count of
  attempts (ten) with a per-attempt window scaled by host speed and **not** by
  width (`round_trip`). A shell echoing a line it has not run yet is
  microseconds of guest time whatever share of the machine it has.
- **`screen_pager_keys`** is paced: one PageDown, then the page it moved, then
  the next. Its verdict was `moved >= (elapsed/3 + 1) * 3`, an arithmetic that
  asks a guest given no time to repaint once for 3.3 moves, and it reported
  `0 page moves over 30 keystrokes in 0.3s`. Unpaced it was also wrong about the
  wire: 60 scancodes into QEMU's 16-byte `PS2_QUEUE_SIZE`.
- **A blown guard is named.** Such a red carries `STALLED:`, prints as `STALL`,
  and is counted apart in the summary. Still red. Gate:
  `stall_is_not_a_verdict`.

### 5.8.2 The measurement

Eight full suites, one session, 2026-08-08: two concurrent twelve-wide runs at a
time from one worktree with `--host-slots 0`, four on `main`'s `tests/toyos.rs`
and four on the branch's, the guest image byte-identical between them.

| arm | suite wall clock | red suites |
|---|---|---|
| `main` | 250, 520, 472, 474 s (mean 429) | 3 of 4 |
| branch | 202, 214, 202, 209 s (mean 207) | 2 of 4 |

Per test, seconds, across the four suites of each arm:

| test | `main` | branch |
|---|---|---|
| `desktop_audio_client` | 17, 14, **FAIL 307**, **FAIL 305** | 20, 17, 17, 17 |
| `desktop_typing_damage` | 18, **FAIL 303**, 18, 16 | 15, 17, 17, 16 |
| `blocked_dump` | 4, 8, 5, 3 | 5, **FAIL 2**, 5, **FAIL 2** |
| `screen_pager_keys` | 14, 14, 14, 14 | 14, 14, 14, 14 |
| `screen_console_scroll` | 14, 9, 15, 13 | 16, 15, 11, 13 |
| `screen_blocked_dump` | 8, 5, 6, 7 | 6, 4, 6, 7 |
| `metal_sim_compositor_stall` | 13, 15, 13, 13 | 13, 14, 13, 13 |
| `metal_sim_client_death` | 4, 4, 4, 4 | 4, 4, 4, 4 |

Pacing `screen_pager_keys` is free: the old loop already paid one screendump per
keystroke, which is about what a repaint costs.

### 5.8.3 What the guard was hiding

**Every one of `main`'s three reds was the `/bin/terminal` boot race**
(`specs/issues/kernel/`), and every one was reported as `nothing typed at the terminal
window reached a shell` with `ALONE: GREEN` under it. Every red suite in the
session — both arms — carried that race in a boot log, and every green suite did
not.

So the class this wave set out to kill was, in this session, one real guest
defect wearing its clothes: the test waited out a ready marker that
`exit: terminal pid=1 code=1` had ruled out at 0.6 s, spent 300 s of a lane
doing it, and reported the symptom. `shell_echoes` ends on that line too now and
names the race, which is the 307 s → 2 s in the table above and most of the
429 s → 207 s beside it.

The general lesson, and it is the one `ALONE: GREEN` invites an agent to miss: a
verdict that expires on the host's clock does not merely fail on a busy host, it
**cannot report anything else** — so every defect underneath it arrives wearing
the same sentence.

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

## 5.9 Wave 8: the kernel every test boots, and the one none of them did

The brief was the owner's: collapse the kernel's test feature flags and split the
guest suite on the result. What the sweep found first is that the premise had a
hole in it in the other direction.

### 5.9.1 Nothing in this suite had ever booted the shipping kernel

`qemu::fold_inert` began its answer with `vec!["test-actuators"]` and added the
request to it. Every image the harness built therefore carried the actuator
umbrella — all 45 kernel builds of a full run, the plain one included. **The
binary an image ships was booted by nothing.** §5.4.4 recorded the cost of the
fold as "a test kernel carrying `SYS_DEBUG` arms it does not use" and judged it
acceptable; the entry did not notice that the same sentence removes the shipping
kernel from the suite entirely.

Two consequences, both live:

- **`test-heap-ceiling` was not inert and §5.4.3 said it was.** Besides its three
  `SYS_DEBUG` arms it moved `MAX_SYSINFO_THREADS` from 65,536 to 16 — a bound
  `SYS_SYSINFO` compares against on every call, from any process. Folded into
  every kernel, that made 16 the bound in **every guest this suite has booted**,
  and the shipped 65,536 was executed by nothing at all. Four shared-boot
  programs call `sysinfo` (`allocator_stress`, `shm_release_reclaims`,
  `abuse_connect_flood`, `audio_idle_suspend`) and have been passing against it.
- **Two shared-boot tests depended on the fold without saying so.**
  `abuse_kernel_addr` (actions 10 and 11) and `tlb_shootdown_waits` (12 and 13)
  run on the plain boot and reach arms only `test-actuators` compiles. Neither
  registration named a feature; both worked because every kernel had one.

The rule the ledger's criterion needs, and did not have: **an inertness claim is
about the whole feature, and it expires the moment the feature grows a second
site.** `test-heap-ceiling` was admitted correctly and became wrong later, and
nothing in the tree could see it. What replaces it is not a better claim but a
different mechanism — a bound that moves is armed at runtime (`SYS_DEBUG` action
14), so there is nothing for a build to carry.

### 5.9.2 What a kernel feature costs, measured

Isolated on the dev host, 2026-08-10, `cargo build` of `kernel/` alone at
`[profile.toyos]`, another worktree building throughout (so these are the loaded
regime, which §5.1 argues is the usual one):

| what | wall | user CPU |
|---|---:|---:|
| the first build of the tree (deps included) | 7.72 s | 36.18 s |
| a cold feature variant, six of them | 6.65–7.48 s | 29.2–30.9 s |
| re-selecting a variant cargo already has | 0.75–0.84 s | 0.57–0.61 s |

**A cold kernel variant is ~6.9 s of wall clock and ~29.6 s of CPU; a warm one is
free.** §1.5 measured ~4–5 s and ~20 s a week earlier, on a smaller kernel.

A full run makes **45 distinct kernel builds** (static census of every
`kernel_features` site and the constants behind them, `[]` included). After a
`kernel/` edit every one of them is cold:

- **302–338 s of wall clock if serialised**, against a ~110 s suite (§5.5.4).
- **1,314–1,391 s of CPU**, on a 14-core host that is also running the suite.

That is the real shape of §1.5's spread and it is larger than §1.5 thought: the
kernel-feature build tax is **two to three times the whole suite**, paid by
whoever runs first after any kernel change, and CI pays a share of it in each of
twelve shards. Each new actuator adds ~6.9 s and ~29.6 s to that bill forever.

### 5.9.3 The sweep: all 52 declared features against the criterion

§5.4.3 asked whether a kernel carrying the feature boots, schedules, drives its
devices and panics identically to one that does not. The answer for the whole
list, by what would have to change for the feature to stop being a build:

| class | count | features | what it would take |
|---|---:|---|---|
| **the one test feature** | 1 | `test-actuators` | nothing — it *is* the collapse. Seven names (`test-fatal-halt`, `test-screen-graffiti`, `test-double-fault`, `test-heap-ceiling`, `test-kernel-canary`, `test-tlb-ack-delay`, `test-idle-guard`) are now arms of it, and `SYS_DEBUG` itself is compiled only under it |
| **armed before userland exists** | 33 | every `i8042-*`, every `xhci-*`, `usb-*`, `rtc-*`, `iommu-*`, `fat-*`, `no-ap-control-regs`, `test-early-panic`, `test-late-panic`, `test-input-merge`, `test-tiny-va`, `test-small-caches`, `log-rotate-fast`, `debug-wait` | a **boot parameter** — §5.9.5. Each acts during init, a port scan, a probe or a clock read, so no syscall can arm it in time |
| **a diagnostic build** | 6 | `heartbeat`, `diag-tick`, `hda-probe`, `metal-panic-probe`, `io-depth-probe`, `control-regs-bench` | nothing, and deliberately. These are a second *product* a human flashes (`cargo run -- --diag-boot --kernel-feature …`, `src/CLAUDE.md`), not a state a test stages. Folding them into the test feature would put `SYS_DEBUG` in a diagnostic image |
| **cannot be runtime** | 2 | `fpu-save-nothing`, `sched-check` | nothing. The first removes the `fxsave64`/`fxrstor64` from `arch::entry`'s bracket — a runtime branch there is on every Ring 3 transition *and* is the thing under test. The second forwards to `toyos-sched/check`, a dependency's cargo feature, which no runtime value can reach |
| **not a kernel build** | 1 | `loom` | declared so `cfg` checking knows the name; `kernel-loom` compiles one file |

**Only one feature in the whole list met §5.4.3's criterion and was not already
folded** — `test-idle-guard`, whose two sites are action 9's arm and the
`percpu::idle_guard_byte` it calls. Collapsing what the existing criterion admits
is therefore worth exactly one kernel build, and that is the honest answer to
step 1 of the brief: **the inert class is exhausted.** Everything left needs a
different mechanism, which is what §5.9.5 designs.

### 5.9.4 The split

- **`test-actuators` is the boundary and the only test feature left.** A kernel
  without it has no `SYS_DEBUG` at all: the number falls to the dispatch's
  default and answers `InvalidArgument`, exactly as an unassigned number does.
  That closes `sys-debug-ungated` — actions 0, 1 and 2 were a diagnostic-channel
  DoS reachable by any process on every image, and there is now no image anyone
  ships that carries them.
- **`fold_inert` is deleted.** A boot builds exactly the features it asked for,
  so a test that asks for none boots the staged artifact `cargo run --build-only`
  writes. `src/build.rs`'s `kernel_key` is the one function both paths key
  through, and `a_boot_that_asks_for_no_feature_gets_the_shipping_kernel`
  (`cargo test --lib`, no build) asserts they agree — with a negative control, so
  a key that ignored its features could not satisfy it.
- **The shared boot is two boots.** `ACTUATOR_TESTS` names the three that reach
  `SYS_DEBUG` — `panic_recovery` through `test_panic_child`, `abuse_kernel_addr`,
  `tlb_shootdown_waits` — and everything else runs on the shipping kernel. One
  extra boot, ~3.7 s (§1.3), against 150 tests that now exercise the binary users
  get.
- **The gate is two-sided and asks the binaries, not a list.** `suite_split`
  reads `tests/toyos-rust-tests/src/bin/*.rs`, finds every program that reaches
  `SYS_DEBUG` directly or through a child it spawns, and refuses both an
  unlisted one — it would run where the syscall answers `InvalidArgument`, and a
  test whose verdict is that a process died would fail for a reason with nothing
  to do with its subject — and a stale entry, which is a test held off the
  shipping kernel for nothing. It carries its own staged input, so the check
  cannot degenerate into a spelling of `true`.
- **The run counts its own boots.** `qemu::boot_census` is two relaxed
  increments; §6 records the boot count as static analysis and a lower bound, and
  a registration is not a boot.

**What the split does not buy is time.** 45 distinct kernel builds before, 45
after: `fold_inert` merged one build (`test-idle-guard`'s) and split one off
(the shipping kernel), and the rest are machines a boot really is. The build
collapse is §5.9.5 and it is a separate landing.

**One thing found and left alone.** `test_screen_graffiti` was in the shared
registry, where it asked the kernel to paint over a panel nobody was reading and
passed on its exit code — a verdict its exit code cannot carry, the same shape
§5.2 found in `test_screen_churn`. It is driven by `screen_console_clear`, so it
is in `RUST_SKIP` now.

### 5.9.5 The boot parameter, designed and not built

Thirty-three features act before userland exists, so `SYS_DEBUG` cannot arm them
and only a parameter the kernel has at `_start` can. The mechanism:

- **Wire format.** A comma-separated list of `name` or `name=value` tokens,
  ASCII, `[a-z0-9-]`, written by the image builder to `\toyos\cmdline` on the ESP
  — beside `\toyos\log.guid`, which is the same shape and the same producer.
- **Who parses it.** The bootloader reads the file if it is there and passes it
  in `KernelArgs`; the kernel parses it into a static arm-set once, before
  `mm::init`, so `test-early-panic` is expressible. Reads are plain loads of a
  word written before the APs start.
- **An unknown parameter is refused by name** — a panic naming the token, on a
  kernel that has the parser and on one that does not, because a shipping kernel
  handed a test parameter must not boot pretending it was given nothing. That is
  the same rule `--kernel-feature` already runs on (`src/build.rs`'s
  `declared_kernel_features`), one layer further in.
- **What it collapses.** 45 kernel builds become 8: the shipping kernel, the
  actuator kernel, the six diagnostic builds, and `fpu-save-nothing` — which is
  ~55 s of cold build against ~310 s, and ~240 s of CPU against ~1,350 s, on
  every run that follows a kernel edit.

**It needs a `toyos-abi` change and therefore its own pull request, ahead of
everything else.** `KernelArgs` lives in `toyos-abi/src/boot.rs`; the sysroot
witness hashes that directory, so the field is an ABI change by the rule root
`CLAUDE.md` states — it claims the sysroot, refuses every other worktree while it
is held, and `--pr`'s `abi-split` check refuses a branch that mixes it with
dependent work. Three alternatives were considered and rejected: a `cmdline` file
in the initrd busts the initrd memo once per parameter set, the kernel cannot
mount the ESP itself until long after the earliest arm has to fire, and
overloading `init_program_addr` is a sentinel by another name.

**Built on 2026-08-10 and §5.9.7 is the record.** Two things the design above
did not have: `name=value` was dropped — every one of the 47 is a boolean and a
signature that parses a value nothing supplies is a promise the code does not
keep — and the feature that carries the parser is `boot-actuators`, separate
from `test-actuators`, so a diagnostic image the owner flashes can arm
`heartbeat` without also gaining a debug syscall.

### 5.9.6 What the sweep found beyond the brief

- **`wall_clock_refusals` is five boots and five kernel builds in one
  registration**, already filed, and it is the one job in the parallel phase that
  `longest_first` can order and can never split. Under §5.9.5 it stays five boots
  and becomes one build.
- **The committed duration profile is CI's, and its numbers are five to twenty
  times this host's** — `boot_partition_identity` 246,953 ms against a suite that
  finishes in 110 s. It is the right profile for the twelve-way CI split, which
  is what writes it; whether it is the right one for ordering this host's
  parallel phase has never been asked.
- **`toyos-abi`'s `debug` wrapper still documents actions 0–3 as if they were
  always there.** Left alone deliberately: touching `toyos-abi/src` for a comment
  would make this an ABI branch and claim the sysroot.

### 5.9.7 Wave 9: two kernels

The owner took §3.6 on 2026-08-10. What landed:

- **`kernel/src/actuator.rs` declares all 47 actuators** — the wire name, the
  accessor the kernel calls, and the claim each rests on, which is the comment
  `kernel/Cargo.toml` used to carry. `kernel/Cargo.toml` is left with six
  features and a test that says so by name.
- **`boot-actuators` is what a build decides about them, and nothing else.**
  With it: a `NAMES` table, one `AtomicU64`, and an accessor per actuator that
  is a plain relaxed load. Without it: no table, no word, no parser, and every
  accessor is `#[inline(always)] const fn … { false }` — so `vma::alloc_floor`,
  both disk-cache ceilings, `log_file::max_log_bytes` and the i8042's report
  length fold to the constants they were, and `init` *refuses* a non-empty
  parameter rather than ignoring one.
- **`test-actuators` stays separate**, because a diagnostic image the owner
  flashes wants `heartbeat` and `diag-tick` and must not also gain `SYS_DEBUG`.
- **The parameter rides `KernelArgs`** (§5.9.5), parsed at the top of
  `kernel_main` after `serial::init` and before `test-early-panic`'s site, which
  is the earliest of the 47. It is our own image builder's string through our
  own bootloader, so an unknown token panics by name: no trust boundary is
  crossed anywhere on that path.
- **`assert_actuators_match_features` asks the artifact, both ways**, in
  `assert_overflow_checked`'s place and for its reason: a shipping kernel must
  name none of the 47 and the test kernel must name all of them, which is what
  keeps the search from being a spelling of `true`. Measured on the two binaries
  this build produces — 0 of 47 at 3,910,240 bytes and 47 of 47 at 4,339,640.
  The names come from `kernel/src/actuator.rs`, so deleting one takes its
  command lines with it, exactly as `declared_kernel_features` does for a
  feature.
- **A run counts its own kernels.** `qemu::boot_census` returns the set, the
  runner prints it, and `DECLARED_KERNEL_BUILDS` refuses a boot that asks for a
  build outside the three.

#### The count, and what it cost

**45 before, from a static enumeration of every `kernel_features` site in
`tests/toyos.rs` and `tests/common/*.rs` and the constants behind them** — which
is §5.9.2's number, re-derived independently here. Two things a partial scan
gets wrong and are worth writing down: the count is not reachable from
`tests/toyos.rs` alone (nineteen of the sets are constants in `tests/common/`),
and four of the sets name **two** actuators — `usb-storage-gate` with each of
`usb-short-read`, `xhci-slow-connect` and `usb-transport-break`, and
`rtc-no-century` with `rtc-century-next`.

**Three after, and a run says so itself**: `ls target/kernel-*` after a full
`cargo test` is three files, and the run's own last line reads
`3 kernel build(s): ["", "boot-actuators,test-actuators", "fpu-save-nothing"]`
over 134 guests. The census is not a static count and cannot be satisfied by
editing a list.

**What the 42 cost, measured on this host on 2026-08-11.** One pass over every
distinct kernel feature set, each set being its own cargo fingerprint, after
touching `kernel/src/main.rs`: **302 s for 43 sets at `origin/main`** against
**9 s for 3 on this branch**. (43 rather than 45 because that scan resolved two
fewer constants than the hand enumeration; main's own dependency rebuild after
the branch switch is inside the 302.) A full green `cargo test` on this branch
after a kernel edit is **469 s**, so the suite that used to pay ~300 s of it to
cargo now pays nine.

**The suite-level A/B the brief asked for could not be taken**, and the reason is
structural rather than an omission: `toyos-abi` differs between this branch and
main, so the two cannot share this host's sysroot, and a checkout at `origin/main`
is refused a build here for as long as this branch holds it. The number above is
the term that changed, measured on both trees by the same procedure; the rest of
the run is the same tests.

**Three after**, and the third is declared:

| build | who asks | why it is not one of the other two |
|---|---|---|
| `""` | every test that needs no perturbation, `cargo run`, every image | it is what ships |
| `boot-actuators,test-actuators` | every actuator test, and the `SYS_DEBUG` shared boot | the actuators, and the debug syscall |
| `fpu-save-nothing` | `fpu_isolation_negative` | below |

`fpu-save-nothing` empties `arch::entry`'s save/restore bracket, which is
`naked_asm!` expanded into every ring-transition stub. A boot parameter there is
a branch on the path the gate is about, in a binary the shipping one no longer
resembles — the gate would then certify a bracket nothing ships. It keeps its
build and `kernel/Cargo.toml` says so where it is declared.

Two more features remain and neither is a build a `cargo test` makes:
`sched-check` forwards to `toyos-sched/check`, a dependency's cargo feature that
no runtime value can reach, and `debug-wait` is what `cargo run -- --debug`
compiles in — a parameter would have to put it in the shipping kernel, and what
`--debug` exists to debug is the kernel an image ships.

#### What the shipping kernel cost

**304 bytes**, from 3,829,136 at `cb52cdb` to 3,829,440, both linked on the dev
host at `[profile.toyos]` and both with 0 of the 47 names in them. It is the
`KernelArgs` field — 16 bytes of struct, its `Debug` arm, and the refusal
`actuator::init` carries for a parameter this binary cannot honour. Nothing else
reaches it: every accessor is `const fn … { false }`, so each call site is an
`if false` the optimiser removes along with the arm behind it.

#### What the negative gates still prove

The owner asked for this walk by name, because a gate whose actuator moved into
the test kernel now proves that the *test* kernel's verdict has teeth. That is
acceptable only where the verdict is shipping code and only the actuator is
test-only.

| gate | the verdict under test | the actuator | after |
|---|---|---|---|
| `control_regs_negative` | `control_regs`' per-CPU line and its two assertions | skip the two register writes on an AP | **unchanged in kind.** The verdict is shipping source; `control_regs` and `control_regs_verdict` run it on the shipping *binary* |
| `iommu_context_absent`, `iommu_empty_domain` | the DMA-fault handler and its report | leave one function out of the root table / give it an empty domain | **unchanged in kind**, same reason |
| `xhci_deaf_controller`, `xhci_deaf_port` | `settles`' deadline and refusal | starve one register wait | **unchanged in kind** |
| `i8042_fault` | the ISR's 16-byte bound and the quarantine path | make the output-buffer check lie | **unchanged in kind** |
| `fpu_isolation_negative` | `arch::entry`'s bracket | empty it | **unchanged, and that is why it kept its build** |

**What is genuinely weaker, stated rather than papered over.** Each of the four
"unchanged in kind" rows certifies the verdict *as compiled into the test
kernel*. That binary carries branches the shipping one does not — `if
actuator::x()` at every site, folded away in the shipping build — so its
inlining and layout are not the shipping binary's. For a verdict that is a
comparison or a log line this is not a difference anybody can name; for one that
is a *duration*, it is. None of the five is timed. The positive halves
(`control_regs`, `control_regs_verdict`, `fpu_isolation`, the ordinary xHCI and
i8042 boots) all run on the shipping kernel, which is the coverage that actually
moved and it moved the right way.

**The refusal has teeth, and it proved it on this landing's own harness.** The
conversion's bulk rename moved one site it should not have: `Task::Shared`
handed `ACTUATOR_KERNEL` — a *feature* list — to `kernel_params`, so every
`SYS_DEBUG` test booted with `\toyos\cmdline` reading
`boot-actuators,test-actuators`. All six died before serial came up, with

```
!!! EARLY PANIC !!!: boot parameter "boot-actuators": this kernel declares no
such actuator
```

which is §5.9.5's "an unknown parameter is refused by name" doing exactly what
it was designed for. Two fields of the same type are swappable, so
`build_boot_image_with` now asks `kernel/src/actuator.rs` which kind each name
is and refuses before a guest is started — the guest's refusal is right and
late.

**Two things this change gives up and nothing replaces.**

Before, a boot with an actuator was the shipping kernel plus one live path;
nothing now checks that the test kernel with *nothing* armed behaves as the
shipping kernel does. The `SYS_DEBUG` shared boot is that machine and 150 tests
do not run on it, so a divergence would show as a broad red rather than a named
one.

And `hda_probe`'s boots used to run the kernel binary a diagnostic image ships.
A flashed diagnostic build is `boot-actuators` alone; the suite has two kernels
and the actuator one carries `test-actuators` with it, so that test now boots a
kernel with a debug syscall the flashed one does not have. Giving it back is a
third build in every run, which is the trade §3.6 is about. The *image* — the
diag config, no test binaries, nothing that can claim the framebuffer — is still
the flashed one.

---

## 7. The nightly tier: what the fast path stopped gating, 2026-08-11

**The owner's decision, in his words: "for now just disable the tests that cost
the most and write it down. we will do it with the nightly ci task."** The line
is his too: **the fast per-PR path runs tests taking ten seconds or less**, and
everything above it goes to a nightly tier that task #188 builds. This section
is the "write it down" half. It is the record the owner reads to decide whether
the interim is acceptable, so it names every test by what the tree loses while
that test is not gated per pull request — not by what the test does.

**This changed no assertion and optimised nothing.** §3.7 is the audit's
standing position that selective test running is the owner's call and that the
audit's own answer is no; this is the owner overriding that for a stated
interval, with the mechanism built so the override is loud and its end state is
one command. #188 holds the postponed optimisation work — the reason to reach
for it is that a name below becomes fast enough to come back, and that reason is
better than a nightly job.

### 7.1 The mechanism

- `src/tiers.rs` is the declaration: `RELEGATED`, one row per test, carrying the
  measured cost, why it is out, and what it guarded. `cargo test --lib` holds
  every row against `FAST_CEILING_MS` and against the committed profile.
- `tests/toyos.rs`'s registration carries `Tier::Fast` or `Tier::Nightly` beside
  each entry's `Sched`, and does not compile without one. `check_registration`
  holds the two lists against each other in both directions at suite startup —
  a registered `Tier::Nightly` with no row, or a row nothing registers, refuses
  the run before anything boots.
- `cargo test --test toyos-build -- --nightly` runs them. That is the flag the
  nightly workflow takes, and the flag anybody touching one of these tests uses.
- **Every run says what it held back**, by name, in the header and again above
  the result line, and the result line itself carries `N held back for the
  nightly tier` — which is the line CI's per-shard job summary extracts.
  `nightly_tier_is_announced` gates both directions of that, because a run that
  quietly does less is the whole failure mode of a tier.
- A filter that matches only relegated names refuses and names `--nightly`
  rather than reporting "no tests match". The tier filter does *not* have an
  exception for filtered runs: a rule with an exception is two rules, and the
  second is the one nobody remembers.

### 7.2 The measurement, and why it is one instrument's

Taken from `target/test-durations` in the primary checkout on 2026-08-11, 310
rows, **1,760.5 s of test time**. Twenty-seven names are over the ten-second
line; four more are under it and go anyway because they share a boot with one
that is over. Between them: **1,347.7 s, 76.6% of all test time.**

**The dev host and CI do not agree about which tests are long, so the line is
applied to one reading and the other is only asked to corroborate.** The
disagreement is not noise and it is not a scale factor — it runs both ways by
more than an order of magnitude:

| test | dev host | CI (`tests/test-durations`) |
|---|---|---|
| `desktop_window_child` | 293.6 s | 63.3 s |
| `metal_sim_pointer_churn` | 16.2 s | 235.2 s |
| `boot_partition_identity` | 4.5 s | 247.0 s |
| `screen_console_scroll` | 49.6 s | 11.1 s |
| `kernel_heartbeat` | 4.9 s | 214.4 s |

Of the 27 dev-host names over the line, **26 are also over it on CI** and the
27th (`launcher_refusals`) postdates the last `--merge-durations`, so the
relegation is not an artefact of one machine. The reverse does not hold at all:
CI prices **44 further names** above ten seconds that the dev host runs in under
ten, which is why `the_committed_profile_corroborates_every_relegation` is a
one-directional gate and says so.

**One reading of one run, and one of the rows knows it.** `desktop_window_child`
is a standing expected failure whose cost is the liveness ceilings it exhausts;
the same profile file was read at 24.8 s and at 293.6 s for it, mtime and size
identical, and nobody has explained the first reading. Its relegation does not
rest on which of the two is right — either way it is the most expensive name in
the suite.

**What the profile will do next.** Once #188's workflow runs the nightly tier
separately, a per-PR shard no longer measures these names, so
`--merge-durations` will report them as "names the profile prices and no shard
ran" and drop them from `tests/test-durations`. The corroboration gate skips a
name the profile has never seen, so it degrades to silence rather than to a
false green — but #188 should merge the nightly run's own shard files, or the
gate loses its second instrument.

### 7.3 What stopped being gated per pull request

Sorted by cost. `src/tiers.rs` carries the full sentence for each; this is the
index and the reason each one is worth a reader's attention.

| test | dev host | what is no longer gated per PR |
|---|---|---|
| `desktop_window_child` | 293.6 s | #156's only reproduction anywhere: a window opened from a shell and closed, and whether the desktop answers afterwards. 17% of all test time by itself. |
| `sshd_fail_closed` | 116.1 s | **sshd once accepted every credential**; this is the gate for that class, including the half a missing-file check passes without — that a daemon which can accept no key is not holding port 22. |
| `fpu_isolation` | 94.5 s | The user machine state surviving every exit from Ring 3, **against a second boot of an `fpu-save-nothing` kernel that must fail the same arms**. A negative gate: the first boot alone proves only that the machine works. |
| `desktop_audio_client` | 80.7 s | An audio client spawned by a shell inside a terminal inside the compositor — the T14's only configuration — a second client connecting mid-stream, and the desktop surviving both. |
| `wall_clock_refusals` | 73.7 s | Every way the wall clock can fail to answer, and the century register's absence. Four boots, a kernel build each. |
| `hda_probe` | 67.5 s | Stage H0 of `specs/hda-driver-plan.md`: every branch of the probe's four questions, plus the plain kernel showing the probe stays out of an ordinary boot. |
| `screen_fatal_halt_composited` | 67.4 s | Whether a fatal panic can paint the panel once a compositor owns the scanout — the T14's only configuration, and the assumption three freeze investigations rested on. |
| `metal_sim_compositor` | 62.6 s | The four daemons surviving the T14's device shape, each in its own words. Nothing supervises any of them, so the message is the entire diagnostic. Carries five more tests on its boot. |
| `doom_music` | 57.5 s | That doom opened the committed SoundFont, played to the end of the check, and reached the device — the three links the host tests cannot make, and the three `b8b0749` broke with the suite green. |
| `doom_sound_flood` | 56.6 s | doom's sound producer outrunning its audio callback without the game dying: the first domino of the T14 freeze. |
| `screen_console_scroll` | 49.6 s | Every row of the panel, character for character, after a workload built to leave stale glyphs. #90. |
| `launcher_refusals` | 46.3 s | **The capability architecture's own enforcement gate**, and endowment landed the day before: sixteen malformed launches at `/bin/init`, with init still launching and the live-object count unchanged. |
| `xhci_msi_only` | 37.5 s | The T14's Thunderbolt controller path, whose "polled mode" did not exist. `msix=off` is the only way the branch executes at all. |
| `control_regs_negative` | 36.4 s | **The negative control** for the control-register verdict: a real divergent AP that `control_regs` must refuse. Without it, whether the verdict has teeth is prose. |
| `desktop_typing_damage` | 23.0 s | What one typed character costs the desktop, off the compositor's own `damage_px_max`. |
| `idle_stack_guard` | 22.8 s | The guard page under every per-CPU idle stack, whose absence is invisible to every log line and every screendump. |
| `dump_nmi_probe` | 22.4 s | Ctrl+Alt+D's NMI probe, and the `rip` it brings back resolved against the kernel's symbols. |
| `metal_sim_pointer_churn` | 16.2 s | Eight plug-and-pull cycles of a pointer under a compositor holding the merged fd — the owner's freeze, which landed on the fourth. |
| `toybox_cp_volume` | 16.1 s | The real `/bin/cp` against a FAT32 volume, including the case where it fills. |
| `usb_boot_stick_pulled` | 15.6 s | #152's only instrument: the boot stick pulled while the desktop draws. The failure has no other witness. |
| `screen_pager_keys` | 14.6 s | PageUp/PageDown reaching the panic console's pager with every CPU stopped. |
| `xhci_deaf_registers` | 14.2 s | Five register spins that had no deadline, which on the T14 is a silent hang and nothing else. |
| `hda_client_stall` | 13.7 s | A client that stops producing mid-stream on both a cyclic ring and a queue, which must answer differently. |
| `metal_sim_compositor_stall` | 13.3 s | A client that stops talking, stops listening, or never stops, with the host watching whether the desktop still paints. |
| `xhci_hid_break` | 10.6 s | A HID endpoint completing with a code the driver dropped where it read it — the Logitech mouse that went silent on the T14. |
| `swiss_german_layout` | 10.5 s | Swiss German end to end by physical key position: the table, the modifier levels, the ISO key and the dead keys at once. |
| `iommu_discovery` | 10.2 s | Four machines differing in one advertised capability each, and whether the kernel's decode moves with them. |

**And four that are under the line and go anyway**, because `group_of` makes a
run of adjacent names one guest and a group cannot be split: `metal_sim_client_death`
(4.0 s), `metal_sim_window_caps` (0.5 s), `metal_sim_ipc_hostile_peer` (0.1 s)
and `metal_sim_scanout_wc` (0.0 s) ride `metal_sim_compositor`'s boot.
`check_registration` refuses a group whose members disagree about their tier,
for the same reason it refuses one that disagrees about `Sched`: the boot would
land in the fast tier for whichever member ran first and charge it the whole
group's cost. That is 4.6 s of collateral against 75.9 s of group, and it is the
part of this change with the worst ratio.

### 7.4 What was deliberately not touched

- **Gate A.** Four boots in every `cargo test`, the owner's floor for anything
  certifying audio, and not a row in the duration profile at all. Out of scope
  by instruction.
- **`EXPECTED_FAILURES`.** Both standing entries survive unchanged.
  `desktop_window_child` being relegated does not touch its exemption, and
  because that entry is `Stale::OnThisDate` rather than `Stale::OnAPass`, it
  still expires on 2026-09-06 whether or not the test has run since — `expired`
  is computed from the declaration and the day, never from what ran. Had either
  entry been `OnAPass`, relegating it would have disabled its own staleness
  detection, and that is the trap to check before relegating anything else.
- **Every assertion, every negative gate, every actuator control.** Two of the
  five scheduler negative gates and one actuator negative control are in the
  relegated set (`control_regs_negative`, `fpu_isolation`); relegating one is a
  scheduling decision and none of them was weakened.
- **The `i8042_keyboard` group.** All three members are under the line
  (7.7 s, 5.0 s, 1.0 s), so it stays whole in the fast tier even though the
  group totals 13.7 s. The line is applied per registered test, which is the
  unit the profile measures.
