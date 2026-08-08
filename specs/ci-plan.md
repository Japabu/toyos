# Continuous integration on GitHub

Everything here was measured on GitHub-hosted runners against `Japabu/toyos`
(public, so Actions minutes are free and unmetered, and 20 jobs may run at
once). Run ids are given so every number can be re-read.

## 1. What a runner is

Measured 2026-08-07, run `31192565891`, five labels in one matrix.

| label | CPU | cores | RAM | free disk | `/dev/kvm` | QEMU from apt/brew |
|---|---|---|---|---|---|---|
| `ubuntu-latest` (24.04) | AMD EPYC 9V74 | 4 | 15 GiB | 88 GB | present | 8.2.2 |
| `ubuntu-24.04` | AMD EPYC 7763 | 4 | 15 GiB | 88 GB | present | 8.2.2 |
| `ubuntu-22.04` | Xeon Platinum 8370C | 4 | 15 GiB | 87 GB | present | 6.2.0 |
| `ubuntu-24.04-arm` | (arm64) | 4 | 15 GiB | 109 GB | **absent** | 8.2.2 |
| `macos-latest` (arm64) | — | — | — | 97 GiB | absent | 11.0.3 (brew) |

Installing QEMU costs 17–48 s from apt and 22 s from brew, so nothing here
caches it. Deleting the preinstalled Android SDK, dotnet, ghc and boost buys
19 GB (88 → 107 free) in 170 s, which the toolchain build takes and nothing
else needs.

`ubuntu-22.04`'s QEMU 6.2 predates `virtio-sound-pci` (QEMU 8.2), which every
audio boot in this tree uses. It is out.

**Disk is not the constraint the plan assumed.** 88 GB free on a standard
runner against the 14 GB the brief expected, and a full bootstrap leaves 82 GB.

## 2. KVM: available after one chmod

**The first probe read as a flat no and was not.** `/dev/kvm` exists on every
x86_64 Ubuntu runner as `crw-rw---- root:kvm`, and the `runner` user is not in
the `kvm` group, so opening it is `Permission denied` and
`qemu-system-x86_64 -accel kvm` fails with `failed to initialize kvm:
Permission denied`. That is a permission, not an absence, and the runner has
passwordless sudo.

Run `31192966156`, `ubuntu-24.04`:

```
crw-rw---- 1 root kvm 10, 232 /dev/kvm      # before
crw-rw-rw- 1 root kvm 10, 232 /dev/kvm      # after sudo chmod 666 /dev/kvm
OPEN-RW: yes
KVM-START-EXIT: 0
KVM-HOST-CPU-EXIT: 0                        # -cpu host -smp 4 -m 2048
```

So hardware virtualisation is **there on x86_64** and **nowhere else**:
`ubuntu-24.04-arm` has no `/dev/kvm` node at all, and `macos-latest` reports no
`kern.hv_support` and aborts on `-accel hvf`. An aarch64 guest running natively
is not available today, which will matter when ToyOS has an ARM64 port and does
not now.

`kvm_amd` and `kvm_intel` are both loaded depending on which host a job lands
on, so a run can be scheduled onto either vendor. The host CPUID as read from
`/proc/cpuinfo` offers `constant_tsc nonstop_tsc pcid rdtscp smap smep xsave
avx2` and does **not** list `x2apic`, `tsc_deadline_timer` or `invtsc`; the
harness already asks for `-cpu host,+rdrand,+smap,+fsgsbase,+x2apic,+smep`, so
KVM's in-kernel APIC supplies the one the kernel needs.

**Which vendor a job lands on is a variable and not a detail** — §7 turned out
to be an AMD-only defect, so a KVM job that draws Intel reproduces nothing and
proves nothing about that class. Nothing can select the vendor, so any KVM job
prints its `model name` as part of its own output.

§7 is closed.

**The shards run KVM. The owner decided it on 2026-08-08**, overruling the
paragraph this section used to end with — that TCG kept the shards comparable
with the dev host's numbers, and that the KVM job beside them was enough to
cover the instructions TCG only emulates. His reasoning:

> "kvm sounds perfect everything should work under emulation and kvm if it
> doesnt something with the guest is wrong."

The comparability point remains true, and what changes is which two things are
being compared. A wall clock on TCG is mostly the emulator's; on KVM it is
mostly the guest's. The dev host's 243 s is comparable with another cross-arch
TCG run on the dev host, and no runner was ever going to be that — §7.1 is the
proof, four cores emulating x86 on x86 against fourteen emulating x86 on arm64.
So CI's number is now a native one and is read as a different instrument rather
than as a slower copy of the same one. The `tcg` job beside the shards keeps the
emulated path from rotting: one test, `/dev/kvm` left exactly as it ships, and
it is the only thing in CI that boots `-cpu qemu64`.

**A defect this uncovered on the way.** Both KVM checks (`src/qemu.rs`,
`tests/common/qemu.rs`) asked `Path::new("/dev/kvm").exists()`. Presence is not
permission and the two are indistinguishable from `exists`, so on an unmodified
runner — and on any Linux box whose user is not in the `kvm` group — every boot
would take `-accel kvm` and die. `toyos_build::kvm_usable` opens the device
instead, and both callers use it.

## 3. The toolchain

The sysroot is host-specific: `rust/build/<host>/stage2` is 790 MB of
`aarch64-apple-darwin` on the dev host and useless on a Linux runner, so a
runner needs an `x86_64-unknown-linux-gnu`-hosted one. Its product is that plus
`rust/build/x86_64-unknown-toyos/stage2`, the ToyOS-hosted rustc that
`system.toml`'s `hosted-rustc = true` puts in the initrd.

Two things had to be true and both were measured rather than assumed.

**Bootstrap does not build LLVM.** `write_config` writes `profile = "compiler"`,
whose defaults include `download-ci-llvm = true`, and the dev host's `ci-llvm`
directory is 493 MB of downloaded artifact. So the cost is rustc, not LLVM.

**But `download-ci-llvm` fails on a runner for a reason that is about forks.**
Run `31193469406`:

```
downloading https://ci-artifacts.rust-lang.org/rustc-builds/4fe0ba68111c.../rust-dev-nightly-x86_64-unknown-linux-gnu.tar.xz
curl: (22) The requested URL returned error: 404
ERROR: failed to download llvm from ci
```

`4fe0ba68111c` is *our* "Merge upstream rust-lang/rust main" commit, which
rust-lang CI never built. Bootstrap picked it because `GITHUB_ACTIONS=true` puts
`check_path_modifications` on its own CI branch, which assumes HEAD is a PR
merge commit and takes `HEAD^1` as the most recently merged upstream commit
(`src/build_helper/src/git.rs`, `get_closest_upstream_commit`). Off CI it walks
back to the newest commit authored by bors, which is a real upstream commit with
a prebuilt LLVM. **`env -u GITHUB_ACTIONS -u CI` is the whole fix**, and it makes
the runner do exactly what the dev host does.

The submodule clone is full-history for the same reason — 101 s, and a shallow
one contains no bors commit to walk back to.

**It costs an hour and it works.** Run `31194201422`, `ubuntu-24.04`, cold:

```
RECLAIM-SECONDS: 170        # 88 GB free -> 107 GB
SUBMODULE-SECONDS: 101      # full-history clone of the rust fork
BUILD-ONLY-SECONDS: 3683    # x.py stage 2, the hosted rustc, and the whole
                            # kernel/bootloader/userland/initrd/boot image
PACKAGE-SECONDS: 3          # 1.56 GiB -> 401 MiB, zstd -3 -T0
```

`rust/build` ends at 16 GB and the disk at 82 GB free, so nothing is tight.
`rust/build/x86_64-unknown-linux-gnu/stage2` is 1.3 GB and
`rust/build/x86_64-unknown-toyos/stage2` 364 MB; together they compress to a
401 MiB release asset, against GitHub's 2 GiB per-asset limit.

**The key is the witness's own question, asked of git.**
`git rev-parse HEAD:rust HEAD:toyos-abi/src HEAD:toyos/src HEAD:userland/libc/src`
hashed to 16 hex digits is the release tag, so the artifact is content-addressed:
a branch that changes none of those four downloads in seconds, a branch that
changes one builds its own, and every later branch carrying the same content
downloads what it built. **Publishing is idempotent** — two branches with the
same content both build, and being second is not an error.

**`Owner::Installed`** (`src/toolchain.rs`) is the third answer to "who owns the
toolchain": a checkout whose `rust/build` holds one and whose `rust/` holds no
source to have built it from. Read off the disk rather than declared, because
such a checkout has exactly one thing it can do — check that the toolchain is
the one these sources need — and a flag saying so could disagree with what is
there. It checks the same three things a linked worktree does (stage2 present,
the rustup link pointing at it, the witness matching) and refuses with "publish
a toolchain built from these sources" instead of offering a claim, because there
is nothing here to claim with.

## 4. Sharding

`testargs::Shard` is `--shard <index>/<count>`, applied to the parallel tasks,
the serial tail and gate A's configs after the phases are decided.
Longest-processing-time over the same measured duration profile `longest_first`
already orders the queue by.

**A shard is a host, not a lane.** `--jobs` divides one machine's cores between
guests that contend for them, and `buildlock::guest_slot` exists because two
suites on one host mismeasure each other. A shard divides work between machines
that share nothing, so the two compose and `--host-slots 0` is right on a
runner: there is one suite on that machine and nothing to arbitrate.

Four gates in `src/testargs.rs`, the load-bearing one being that the shards are
a partition — nothing dropped by all of them, nothing run by two — because a
shard that silently owned no tests would report the run green.

**A sharded run may not write the profile it partitioned on.** The partition is
a function of `target/test-durations`, so shards that disagree about that file
disagree about who owns what. Three shards of `nvme_` run back to back in one
worktree: shards 1 and 3 both ran `nvme_home_roundtrip` and `nvme_large_device`
ran nowhere — three runs, three profiles, three partitions, and a green result
from every one of them. A shard therefore does not `save_durations`, which is
also right on its own terms: a third of the suite is a third of a measurement.
Re-verified after the fix — `nvme_large_device`, `nvme_wide_sector`,
`nvme_home_roundtrip`, one each, all green.

CI never had the bug, because a runner has no profile at all and every shard
reads the same nothing. The dev host is where it bites.

**`Duration::MAX` is not a price.** `longest_first` costs an unseen test at
`Duration::MAX` so that it sorts first; a shard adds costs up, and a runner has
no `target/test-durations` at all, so the first sharded run panicked in
`Duration::add` before it booted anything. Saturating arithmetic is the wrong
repair — every bin equals `MAX` after one item each and the rest go to bin 0,
measured 8/1/1 across three shards. `keep` takes `Option<Duration>` and prices
an unmeasured item at the longest that *was* measured, which is the same
conservatism in a form that can be added; where nothing is measured every item
prices the same and LPT degenerates to round-robin.

### `tests/test-durations` is the profile a machine that has measured nothing starts from

Round-robin is what put 191 of 268 tests on one shard of run `31238056513` and
cut it off at its job timeout while another finished in sixteen minutes. Every
runner is that machine on every push, because a fresh clone has no `target/`. So
the profile is committed, `load_durations` reads it first and lays whatever this
worktree has measured on top — **per name, not per file**, so a checkout that
has only ever run a filter keeps the committed number for everything the filter
did not name.

**Its numbers come off a runner, and that is the point.** It is read by the
machines that have nothing else; the dev host overwrites every name it measures
on its first full run. Cross-arch TCG on an M4 Pro and KVM on four Azure cores do
not agree about which tests are long, and a profile taken here would be the wrong
hint for the only reader that needs one.

**§4's rule is intact and is what makes this safe.** A shard still may not write
the profile it partitioned on. What it writes instead is
`target/test-durations.shard-<i>-of-<n>` — its own tests and no others, in a file
`load_durations` never opens — and CI uploads the twelve.
`cargo run -- --merge-durations <dir>` is the deliberate act that turns them into
the committed file, and it exists rather than a `cat` because it can check the
one property the merged file's usefulness rests on: **a name in two shard files
is a run whose shards were not a partition**, which is precisely the defect §4
records and which a concatenation cannot see.

## 5. What runs, on what trigger

| workflow | trigger | runner | what it is |
|---|---|---|---|
| `host-tests.yml` | every push, every PR | `macos-latest` | `cargo test --lib` plus all fourteen host crates and `userland/sshd`. No toolchain, no guest. |
| `toolchain.yml` | every push | `ubuntu-24.04` | Publishes the content-addressed toolchain release, or does nothing. |
| `ci.yml` | every push, every PR | `ubuntu-24.04` ×13, `debian:sid` container | Twelve guest shards on KVM at one lane each, plus one TCG canary. |
| `gate-a.yml` | `workflow_dispatch` | `ubuntu-24.04` ×2 | The thorough audio tier, one audio test per machine. |
| `probe-*.yml` | push to `ci/probe-*` | various | The measurement workflows this document is made of. |

`ci/**` is a throwaway namespace excluded from the first three, so a probe push
does not start a suite.

**macOS for the host tests used to be two things and is now one.** The *judge*
was macOS-only — `fsck_msdos`, on both `toyos-fat32` and `src/image.rs` — and it
is ours now (§6.1). What is left is that `toyos-fat32` still *builds* its volumes
through `newfs_msdos` and `hdiutil`, and that is the whole reason this job is
not on Linux.

The crate list is CLAUDE.md's, whole. It was nine of the thirteen until
2026-08-08, and the four missing — `toyos-elf`, `toyos-ld`, `toyos-pci`,
`toyos-desktop` — are exactly the pure crates whose point is that a decision is
testable without a guest, so CI was skipping the cheapest tests it had.
`toyos-fat32-check` is the fourteenth.

Measured, `macos-latest`, run `31194966771`, cold: 594 s end to end — `cargo
test --lib` 183 s, the nine host crates 274 s, sshd 124 s, all of it
compilation. With the cargo cache: 442 s (run `31201564400`). Those numbers
predate the five crates added since.

`toolchain.yml` when the release exists: **6 seconds** (run `31201563879`).

## 6. What a fresh clone does not have

**The SoundFont — and neither the diagnosis nor the fix this section first
carried survived contact with the licence.** 86 of run `31222412737`'s failures
were `assets/timgm6mb.sf2 ... is not there`, and the same defect in worktree
clothes made every brand-new worktree red on `metal_sim_compositor` and
`metal_sim_pointer_churn` until somebody copied the file across.

Two things were wrong. The first is that a fresh clone lacking it was never the
mechanism: **`userland/doom/build.rs` downloaded it**, so any config that built
doom had it — and of the five configs declaring it in `untracked-assets`,
exactly one built doom. `console`, `desktopcase`, `desktopaudiocase` and
`metalcase` each shipped 5.99 MB into an initrd holding nothing that could open
it, and each turned a hard error on an asset nothing in that image wanted.

**The second is the one that settles it. TimGM6mb is GPL-2.0 and this tree is
MIT OR Apache-2.0**, so every image this project has ever built put copyleft
obligations on whoever redistributed it. The owner's decision, 2026-08-08:
**ship nothing.** The file is out of the repository, `.gitignore` carries the
pattern rather than the one name, and doom's build script no longer fetches it.
Music is opt-in — drop a `.sf2` in as `assets/soundfont.sf2`. `system.toml`
records GeneralUser GS (CC-BY-4.0, ~30 MB) as the recommended one, with its
author's own caveat that he cannot be certain of every sample's origin;
MuseScore_General is MIT but ships as `.sf3`, whose Vorbis-compressed samples
rustysynth does not read. Both are around six times the size of the image's
largest current entry, which is the other reason there is no default.

So the four spurious declarations are gone, which loses nothing — **the config
that builds doom is the config that declares doom's asset** — and the one that
remains declares an absence rather than a requirement: `src/assets.rs` **names
and skips** a declared entry it cannot find instead of stopping the build. The
absence is stated twice, at build time by name and in the guest's log by
`toyos_music_init`'s "playing without music". Nothing is fetched in CI and
`TOYOS_SOUNDFONT_URL` is gone with its step.

Gate A was never affected — it runs on `tests/testcases`, which declares no
untracked asset.

## 6.1 The outside judge is ours now

`fsck_complaints` used to shell out to `/sbin/fsck_msdos` and return `no
/sbin/fsck_msdos: this gate's outside judge is missing` anywhere else — a red,
correctly, rather than a silent pass, and 13 of run `31222412737`'s failures.
Six guest tests reach it (`esp_filesystem`, `kernel_log_file`,
`log_partition_layout`, `log_partition_identity`, the toybox `cp` case) and so
does `cargo test --lib`.

**This section used to recommend `fsck.vfat` from dosfstools. The owner refused
it and the binary it would have replaced**, on 2026-08-08 and on the rule
CLAUDE.md already states: "no dependencies on binaries that dont come with rust
or qemu... we even have our own c compiler and linker because we dont like
dependencies why should we now accept something like that?"

So `toyos-fat32-check/` is a FAT32 volume checker written from Microsoft's
fatgen103 — no dependencies, `forbid(unsafe_code)`, `no_std` + `alloc`, and
deliberately derived from neither `toyos-fat32` nor `fatfs`, because a checker
written from the code it judges shares that code's bugs. `check(&[u8])` returns
typed `Complaint`s carrying their own numbers, which is what let the digit
masking `fsck_complaints` needed go away entirely. Its 66 mutation tests each
corrupt one thing in a hand-built volume and require the matching complaint.

**It is stronger than what it replaces, and that was measured rather than
assumed.** A development-time A/B ran 59 corruptions through both: no arm where
`fsck_msdos` complained and the checker was silent, and twelve where the checker
complains and fsck does not — including the two `specs/known-issues.md` §9
records fsck missing outright, a stale FAT mirror and duplicate 8.3 short names.
The hand-built fixture passes `fsck_msdos -n` clean, so it is a real volume.

**What is still macOS-only is the *formatter*, not the judge.**
`toyos-fat32/tests/` builds its volumes with `newfs_msdos` and populates them
through a `hdiutil` mount, which is the point of that suite — our reader against
bytes we did not write. Replacing those two is a larger job and is scoped in
`specs/known-issues.md` §9. Nothing in the *guest* suite touches them.

`ps -Ao comm=` and `getloadavg` — the other two host calls the harness makes —
are both Linux-native, so with the judge portable there is nothing left in the
guest suite that a Linux host cannot run.

## 7. The KVM finding

The first sharded suite on a runner (run `31201562768`) reds almost everywhere
with one signature:

```
!!! FAULT rip=0xffff80007b519a02 cr2=0x0 err=0x0000000000000018 ... tid=0
KERNEL PANIC: general protection fault (error_code=0x18)
  Syscall: num=86 user_rip=0x100000544ad user_rsp=0xffffffe350
```

Different syscall numbers, different processes, **the same kernel offset**
across boots whose KASLR slides differ. Three things changed at once against the
dev host — the accelerator, the QEMU version and the host CPU — so a probe
changed one of them.

Run `31205890425`, one runner image, one QEMU 8.2.2, one commit, one toolchain,
one test. The only difference between the two jobs is whether the job ran `sudo
chmod 666 /dev/kvm`, which is the question `kvm_usable` asks:

```
accel (tcg)  PASS  process_stats  (115ms)   test result: ok
accel (kvm)  FAIL  process_stats  — KERNEL PANIC: general protection fault (error_code=0x18)
             ... and again on the harness's re-run alone
```

**It is the accelerator — and, it turned out, the vendor with it.** That A/B
changed one flag but the two jobs also drew different hosts, and the defect is
AMD-only: eight runners drawing an EPYC failed 64 boots of 64, and two drawing
an Intel Xeon passed. Both facts are one cause. `STAR[63:48]` held `0x10`, and
`SYSRET` derives SS from it plus 8 — Intel's SDM ORs RPL 3 into that, AMD's APM
does not, so an AMD host ran userland with `SS = 0x18` and the first `iretq`
back to such a thread died on "SS.RPL must equal CS.RPL". QEMU's `helper_sysret`
implements Intel's wording, which is why no TCG guest anywhere — runner or dev
host — can see the class. Fixed in `arch/percpu.rs`; the write-up and the
evidence are `specs/known-issues.md` §3.

**The reusable part is the shape of the blind spot**, not the bug. A guest that
emulates an instruction gives you one vendor's reading of it, and the dev host
has no other. Anything whose correctness depends on which vendor is executing —
`syscall`/`sysret`, segment loading, `iret`'s privilege checks — is gated by the
KVM job or by nothing.

## 7.1 And TCG on four cores does not fit the suite's clocks

The first full guest run on `main` after the shootdown fix — run `31213985427`,
four `ubuntu-24.04` shards, `--jobs 4`, TCG:

| shard | tests | passed | failed | parallel phase | total |
|---|---|---|---|---|---|
| 1 | 203 (carries the 153-test shared block) | — | — | — | **cancelled at the 110 min job timeout** |
| 2 | 32 | 17 | 15 | 677.7 s | 1263.3 s |
| 3 | 27 | 13 | 14 | 813.3 s | 1553.3 s |
| 4 | 27 | 13 | 14 | 1236.3 s | 1824.2 s |

Against the dev host, same tree, same day: **289 tests, 243.4 s, one machine.**

The failure classes, counted over the whole run:

```
 198  timed out after 40s
 109  timed out after 20s
  27  assets/timgm6mb.sf2 ... is not there
  10  timed out after 120s
   9  no /sbin/fsck_msdos
   7  timed out after 80s
   ... and a long tail of 30 s–720 s timeouts
```

**Overwhelmingly timeouts, and they are liveness guards rather than verdicts.**
`qemu::budget` pays them out per guest the phase may have up, which corrects for
*width* and not for how fast the host is; a 4-core Azure vCPU emulating x86 on
x86 is far enough below a 14-core M4 Pro that ceilings written for the latter do
not hold. §6 and §6.1 are 36 of the failures; every other one is the clock.

So **TCG on a standard runner is not a working configuration for this suite**.
Three directions were named here, and all three were taken — the owner chose the
accelerator (§2) and the other two turned out to be the rest of the same answer.

## 7.2 All three, measured

Three runs on `wt/toyos-cifit`, one change at a time.

| run | config | result |
|---|---|---|
| `31222412737` | TCG, 4 shards, `--jobs 4` | shard 1 cancelled at 110 min; 2/3/4 at 17/32, 13/27, 13/27 |
| `31233476555` | **KVM**, 4 shards, `--jobs 4` | shard 1 cancelled at 110 min; 2/3/4 at 21/32, 19/27, 20/27 |
| `31238056513` | KVM, 6 shards, `--jobs 2`, host-scaled ceilings | 1/2/3/4/6 at 189/191, 16/23, 10/18, 15/18, 16/18; shard 5 cut off |

**KVM alone stopped the timeouts being the story.** 307 bare timeouts became a
named failure list. What it did not do is make four guests fit on four cores:
every profile boots `-smp 2` and six tests boot `-smp 8`, so `--jobs 4` is eight
to thirty-two vCPUs on four, and a guest whose kernel spins with `IF` clear pays
lock-holder preemption for the difference. The harness's own re-run-alone column
priced it — `netd_connection_caps` 551 s wide against 6 s alone,
`metal_sim_input` 336 s against 4 s, `desktop_locale_detect` 51 s against 4 s.

**Host-scaled ceilings are the third, and the boot is the measurement.**
`qemu::budget` now multiplies by `fastest_boot / 1320 ms` as well as by the
width, never below 1 and never above 8. The reference is the fastest boot of a
291-test suite on the dev host; a runner's fastest is 1672–2308 ms, so CI pays
1.27×–1.75×. The TCG canary is the demonstration that this was needed at all: it
ran `process_stats` green on one push and `timed out after 5s` on the next, same
command, same commit content — and is green again with the scaling on.

**Shard 1 is the headline: 189 of 191.** The 153-test shared block, which
neither TCG run ever finished, runs in one guest with almost every test under a
second.

### What is left, and it is not the clock

Twenty-two failures over the five shards that finished. The harness classifies
them itself and the classes are worth separating:

- **`ALONE: GREEN` — the classification, not the tree.** `i8042_quarantine`,
  `i8042_fadt_denial`, `hda_client_stall`, `cache_eviction`, `nvme_large_device`,
  `usb_flush_optional`, `screen_console_scroll`, `metal_sim_input`. Every one is
  `Sched::Parallel` and every one is on `specs/known-issues.md` §7's list or
  belongs on it. Two lanes on four cores is still two guests.
- **Input delivery, and this is the real finding.** `desktop_typing_damage` (6
  of 16 echoes, *alone*), `i8042_mouse` ("1007 of the 1007 packets injected came
  back out" and still stalled, alone), `xhci_hotplug` (alone),
  `metal_sim_pointer_churn` ("the churn did not reach the kernel", alone),
  `desktop_window_child` (nothing typed reached a shell, alone), `xhci_flap`.
  All six pass on the dev host in seconds — measured, `--jobs 1`, all eight
  suspects green. Two things differ and neither has been separated yet: the
  accelerator, and **QEMU 8.2.2 on the runner against 11.0.3 here**. Injection
  is a QMP path through the emulated i8042 and xHCI, which is exactly where a
  three-major-version gap would show. This is what the owner's rule is for —
  "everything should work under emulation and kvm if it doesnt something with
  the guest is wrong" — and it is the next thing to settle.
- **Two audio reds.** `hda_tone` with `1 mid-tone silences`, which the #88
  exemption correctly does not cover, and `audio_tone_load (smp=8)` timing out
  with eight vCPUs on four cores. Both are known-issues §4 shapes.
- **`doom_sound_flood`**, timed out at 300 s wide and 104 s alone against 4–26 s
  here. Not input and not obviously the clock.
- **`usb_storage_shapes`** — `the driver did not report "blocks of 4096 B"`,
  alone, in 4 s. Reads like a real difference and not a margin.
- **`metal_sim_compositor`** — `"sshd: no network on this machine, exiting"
  never reached the console`.

Nothing left in the list is a bare timeout at a ceiling nobody chose, which was
the whole of §7.1.

### Still open on the configuration itself

- **The partition is round-robin.** `longest_first` orders on
  `target/test-durations` and a runner has none, so every test prices the same
  (§4). Shard 5 spent 1932 s in its parallel phase and was cut off; shard 6
  spent 411 s. A profile committed for CI to read would fix the partition, and
  §4's rule that a shard may not *write* one is what makes that safe.
- **The comparison §2 wants** is still available and still costs one runner: the
  `tcg` job is one test, and a whole TCG shard would be the other arm.

## 7.3 The 2x2, and the input class was three things

The section above named one finding and it was three. Four jobs settled it:
`ubuntu-24.04` throughout, one commit, one toolchain, `--jobs 1`, the same eight
tests one `cargo test` at a time. QEMU is the axis the container moves and
nothing else does — the firmware is `ovmf/` in this repository on every host, the
boot image is built in the job, and the host CPU is the same Azure vCPU (an AMD
EPYC 7763 in all four). Runs `31245897225` and `31246245541`.

| QEMU | accel | fastest boot | red |
|---|---|---|---|
| 8.2.2 (apt) | KVM | 1.7–2.3 s | `metal_sim_pointer_churn`, `xhci_flap`, `desktop_typing_damage` |
| 8.2.2 (apt) | TCG | 4.4–4.6 s | all but `abuse_gpu_resolution` |
| 11.0.3 (`debian:sid`) | KVM | 1.7–2.3 s | `metal_sim_pointer_churn`, `xhci_flap` |
| 11.0.3 (`debian:sid`) | TCG | 4.0–5.0 s | **none** |

**The two TCG cells are the strongest thing in the table**, because their boots
are the same speed — 4.4–4.6 s against 4.0–5.0 s — and one is green throughout
while the other loses seven of eight. Whatever 8.2.2 does differently to the
emulated i8042, it is not that it is slower.

**Half the class was the width and neither variable.** `i8042_keyboard`,
`i8042_mouse`, `i8042_fadt_denial` and `xhci_hotplug` are green in every arm
above and were red in run `31241099454` on the same runner image under the same
accelerator — at `--jobs 2`. That is §7's contention class, reaching further than
anyone had counted.

**One is the QEMU version.** `desktop_typing_damage`, red on 8.2.2 and green on
11.0.3 under the same accelerator. Three major versions of the QMP injection path
was a variable this tree had never had a second data point on.

**Two are the accelerator, so by the owner's rule they are the guest's.**
`metal_sim_pointer_churn` was the harness sampling a console that had not caught
up, and is fixed — the count waits for its evidence now instead of sleeping 400 ms
for it. `xhci_flap` survives three collapsed replugs and stops on the fourth;
that is a driver defect CI found, written up in `specs/known-issues.md` §8 and
not fixed here.

### What the configuration is now, and what each part of it cost

- **QEMU 11.0.3 in a `debian:sid` container**, which is the dev host's exact
  version. `ubuntu-24.04` ships 8.2.2 and a runner can install nothing newer;
  the image survey (run `31245897225`) is `debian:trixie` 10.0.11, `debian:sid`
  11.0.3, `ubuntu:25.10` 10.1.0, `ubuntu:24.04` 8.2.2. Cost: one `apt-get` and
  one `rustup-init` per job that the ubuntu image gave for free. Buys:
  `desktop_typing_damage`, and CI comparable with the dev host on the one axis
  that is not deliberately different.
- **`--jobs 1` and twelve shards** instead of `--jobs 2` and six. One lane
  removes the `ALONE: GREEN` class by construction rather than by tuning: there
  is never a second guest on the machine. It costs nothing — a shard is a whole
  machine, this repository is public, twenty jobs may run at once and the minutes
  are unmetered.
- **A committed duration profile**, §4.

### The shared block stays whole, and that is measured rather than assumed

It is a single point of failure and it was fixed as one rather than split. Run
`31238056513`, shard 1, the runner's own clock: the block ran from 03:59:22.9 to
03:59:37.1 — **14.2 s for the whole 153 tests, inside a 685.6 s parallel phase**.
Splitting it six ways would buy about twelve seconds of overlap and cost five
more boots and five more per-boot images, which is not a 2x improvement of
anything.

What it *was* costing is the blast radius, and that is a different repair. Run
`31241099454`, shard 1: five tests passed, `abuse_gpu_resolution` took the guest
with it, and the 150 behind it each paid a full liveness ceiling for a machine
that was gone — 65 minutes of nothing and a job cancelled at 90. Two changes,
both in the harness:

- **A shared boot that stopped answering is answered with a new one.** A test
  whose turn came, whose whole ceiling passed and which was never even announced
  is not a test that ran; `TestResult::started` is what says so, off the
  `===TEST_START <name>` the in-guest runner has always printed. Bounded at
  `MAX_SHARED_REBOOTS`, because a block whose every member kills the guest must
  not boot one per test. A reboot rather than an abandonment, because those 150
  tests are each still owed a verdict and a suite that reports 150 reds it never
  ran is worse than one that pays for a boot.
- **A `===TEST_END` naming another test is the previous one's.** The name has
  been on the wire since the runner was written and was parsed and thrown away;
  taking it was `specs/known-issues.md` §6's cascade, where one timed-out test
  made every later member of the block read a window that opened on its
  predecessor's output — 110 of 238 red on an "actual" that was verbatim the
  previous expectation.

### What the new configuration measured

Run `31247206462`, twelve `debian:sid` shards on KVM at `--jobs 1`, still with a
round-robin partition because the profile it would read had not been committed
yet: **271 of 292, every shard finished**, against 246 of 268 on the five that
finished of run `31238056513` and a shard cancelled at 110 minutes in each of
the two before it. Shard 1 carried the 153-test shared block and reported
**180 of 183 in 537.4 s**.

The per-job fixed cost, off shard 9's own step timings: container start 8 s,
`apt-get` 61 s, checkout 2 s, `rustup-init` 8 s, toolchain download 5 s — **85
seconds**, which is what the `ubuntu-24.04` image was giving for free and what
QEMU 11.0.3 costs. The suite step is a `cargo build` and then the tests; shard 9
spent 853 s on the pair for a 556 s suite.

**One configuration defect, and it cost the run two shards.** `what a red run
left` uploaded `/tmp/toyos-tests-*/` whole, and a lane's scratch holds a per-boot
image of a few hundred megabytes: shard 9's suite finished in 556 s and the
upload then ran for **44 minutes** and hit the job timeout, so a job that had its
answer reported as cancelled. It uploads logs and screendumps now.

The twenty-one that were left fall into four groups, and only the first is about
the harness:

- **Eight `ALONE: GREEN` at one lane, which is a different thing from the same
  words at width 2.** Nothing else was ever up, so the two runs differed in
  nothing the harness controls: each failed once and passed once. That is a rate
  and says nothing about `Sched`, and the message says so now.
- **The xHCI plug/unplug family under KVM**: `xhci_hotplug`, `xhci_hid_break`,
  `metal_sim_pointer_churn`, `usb_transport_break`, and `xhci_flap` from the
  probe. Every one drives `device_add`/`device_del` over QMP, every one is red
  again alone, and every one is green under TCG on the same runner image and the
  same QEMU. `specs/known-issues.md` §8. **`usb_transport_break` is closed and
  belonged to the family only by its symptom** — it drives no QMP at all, and
  what it shared with the rest is the one variable: a guest running 50x faster
  wins a race against the device that a TCG guest loses.
- **The `metal_sim` group and the desktop**: `metal_sim_client_death` red alone
  at 354 s, with `metal_sim_window_caps`, `metal_sim_ipc_hostile_peer` and
  `metal_sim_compositor_stall` behind it — the same blast radius the shared block
  had, in `Task::Machine`'s grouped guest, which the shared block's repair does
  not reach. `desktop_typing_damage` and `sshd_fail_closed` are red alone too.
- **Three that are their own**: `doom_sound_flood` (red alone, 92 s),
  `hda_client_stall` (red alone), `metal_sim_null_audio` (`soundd did not present
  a null sink on a device-less machine`, since closed — §9.2), and `hda_tone`'s
  mid-tone silence, which #88's exemption correctly does not cover.

`usb_storage_shapes` is green here and was red under 8.2.2 — the second thing
the QEMU version bought, and it was not one of the eight the probe measured.

### Four tests were running twice under one name, and the merge command found it

`check_registration` compared `MACHINE_TESTS`, `SCREEN_TESTS` and `AUDIO_TESTS`
against each other and never against the binaries the shared registry
*discovers*, so `cache_eviction`, `hda_client_stall`, `audio_tone` and
`audio_tone_load` each produced two outcomes under one name — once on the plain
boot, once as the test that owns the name. `--merge-durations` refused the twelve
shard files on exactly that, which is the check earning its keep on its first
use: two shards had measured `cache_eviction`, at 132 ms and 22.5 s.

Two verdicts under one name is not extra coverage. `retry_task` searches the
shared registry first, so a machine test of one of those names that failed wide
was re-run **as the other test** and its `ALONE:` line described neither. The
four are in `RUST_SKIP` now — each name's own test still runs, on the machine it
needs — and `check_no_collisions` refuses the next one.

## 8. Two instruments, and what each one is evidence for

**They run the same 292 tests and they are not the same measurement.** Nothing
in either one says so, so an agent who reads a CI red as a dev-host red — or the
reverse — draws a conclusion neither machine supports. That is exactly the shape
§7 was: a class of defect the dev host cannot see at all, invisible for as long
as it was the only machine.

| | dev host | a CI shard |
|---|---|---|
| host arch | arm64 (M4 Pro, 14 cores) | x86_64, 4 cores (Azure vCPU) |
| guest | cross-arch TCG | **KVM**, `-cpu host` |
| host CPU vendor | one | AMD EPYC or Intel Xeon, **not selectable** |
| QEMU | 11.0.3 (brew) | 11.0.3 (`debian:sid`) — deliberately the same |
| guests at once | up to twelve, `buildlock::guest_slot` | **one**, `--jobs 1` |
| other load | every other worktree's build and suite | nothing |
| sysroot | one, shared, claimable | its own, keyed on the branch's four trees |

### 8.1 What only CI can answer

- **Which vendor's reading of an instruction the kernel depends on.** A guest
  that emulates `syscall`/`sysret`, segment loads or `iret`'s privilege checks
  gives you one vendor's wording of it, and QEMU implements Intel's. §7 is the
  worked example: `STAR[63:48]` was green on the dev host in every run there had
  ever been and lost 64 boots of 64 on an EPYC. The `kvm` shards are the only
  gate on that class, and the vendor is a lottery, which is why every job prints
  its `model name`.
- **What the guest does at native speed.** `xhci_flap` survives three collapsed
  replugs and stops on the fourth under KVM, and is green under TCG on the same
  runner image and the same QEMU, and green here — because the accelerator runs
  the guest about fifty times further between the host's two QMP writes
  (`specs/known-issues.md` §8). CI found a real driver defect the dev host has no
  way of constructing.
- **A machine that can run these guests natively at all.** The "every component
  within 2× of a production OS" bar is unmeasurable under cross-arch TCG, whose
  distortion CLAUDE.md records at 1.06×–6.5× and non-uniform.
- **No shared sysroot.** Every runner has its own, so the contention
  `specs/worktrees.md` §3.1–§3.2 describes — a claim that refuses every other
  worktree, measured at 35 and 50 minutes of nobody being able to build — does
  not exist there. Observed while writing this: the local guest suite could not
  be run at all for the length of another agent's ABI change.
- **A quiet host for gate A.** Each shard is its own VM, so
  `live_instances() == 0` holds by construction rather than by the suite
  arranging it, and no other agent's build is on that machine. The thorough tier
  is N boots per config strictly one at a time, so a second machine is the only
  thing that can shorten it: `--shard 1/2` and `2/2` put one audio test on each.

### 8.2 What only the dev host can answer

- **Anything about contention.** A shard is one guest on one machine, so
  `HostSlots`, `buildlock::guest_slot`, `qemu::budget`'s width multiplier and the
  whole `ALONE: GREEN` classification are untestable on a runner *by
  construction* — there is never a second guest for the first to contend with.
  `specs/known-issues.md` §7's parallel-red class is a dev-host phenomenon and CI
  says nothing about whether it is fixed.
- **Whether a `Sched::Parallel` is right.** Same reason: the answer requires two
  guests. Every CI retry is the width-1 kind and says only that a test failed
  once and passed once.
- **The suite's own wall clock as a number anyone compares.** 292 tests in 536 s
  on one machine against twelve shards spanning 273–530 s each; those are not two
  readings of one quantity.
- **A second architecture of host, for the parts that are not the accelerator.**
  arm64-on-arm64 is where an ARM64 port would be gated, and there is no runner
  that can do it (§9).

### 8.3 How to read a disagreement

**A red on one and green on the other is a finding about the difference, and
about the tree only once the difference is named.** Both directions have already
happened, and the second is the one that catches people:

- **CI red, dev host green — and the tree was wrong.** §7's `SYSRET`. The dev
  host could not execute the instruction.
- **CI red, dev host green — and the *runner* was the variable.**
  `desktop_typing_damage` on QEMU 8.2.2, green on 11.0.3 under the same
  accelerator on the same runner image. Closed by putting the dev host's own QEMU
  in the container rather than by touching the test.
- **CI red at `--jobs 2`, dev host green at `--jobs 12`.** The whole i8042 family
  in run `31241099454`. "Wider" and "more contended" are not the same axis: every
  profile boots `-smp 2` and six tests boot `-smp 8`, so two lanes on four cores
  is heavier oversubscription than twelve lanes on fourteen. Closed by one lane
  per machine and twelve machines.

The rule that falls out: **before believing a CI red is the tree, name what
differs.** The candidates are short and they are all in the table above.

## 8.4 Somebody had to push `main`, and now nobody does

**Closed by §10**, and not by the one line this section proposed. `main` moves
only through a merged pull request now, so GitHub pushes it and there is nothing
left to fall behind. The account below is kept for the failure mode, which was
worth a section: a default branch nobody tests is a default branch whose CI
result is about a tree that no longer exists.

`cargo run -- --land` fast-forwarded the primary checkout and did not push, and
the owner's rule is that an agent pushes its own branch and never `main`. At the
moment this landed, `origin/main` was **64 commits behind** local `main` and
carried no `.github` directory at all; it was pushed by hand shortly after and
now carries all eight workflows.

Most of CI works either way, because a branch created from local `main` carries
the workflows with it: **a pushed branch runs its own CI, and so does a PR** —
the `pull_request` event resolves workflows from the merge of head and base, and
head has them. Three things need `main` itself to be current:

- `main` is otherwise never tested; a landing's gate is the only thing that ever
  sees it.
- `actions/cache` entries written on `main` are the only ones every branch can
  read, so otherwise every branch's first run is cold.
- `workflow_dispatch` requires the workflow on the default branch, and that is
  how `gate-a.yml` is triggered.

One line in `src/land.rs` after the fast-forward closes all three
(`git -C <primary> push origin main`), and it keeps the rule intact: `main`
would still move only through `--land`. **Still not done, and still the owner's**
— it is a change to the landing protocol and to his rule.

**It bit, and the measurement is 2026-08-08.** `origin/main` was **31 commits
behind** local `main` — every one of them the twelve-shard work — and the newest
`ci` run on it (`31249540044`) is **1 h 18 m and red**, a configuration those 31
commits replaced. Anyone reading this repository's default-branch CI was reading
a stale config's failure.

`--land` was made to *say so* — `origin_main_lag`, a lower bound read off the
local remote-tracking ref, printed with the `git push` that would close it. It
went with the command.

## 9. Not done

- **Gate A's variance on a runner, against the dev host's.** The instrument is
  built (`gate-a.yml`, `--shard` on the audio tier, and the gate already prints
  the whole sorted sample as `toml:` lines), and the recorded spread to compare
  against is in `tests/audio-baseline.toml` — `max_wake_lat_us` 5666–10090 over
  30 runs on `audio_tone.smp1`, `wakes` 881–936. What is missing is the run.
  It was blocked behind §7 and behind an hour-long toolchain build, and the
  question it answers — whether a runner's spread is comparable to the dev
  host's — decides whether the thorough tier can move. **Do not move it until
  that number exists.**
- **An ARM runner running aarch64 guests.** Not possible: no `/dev/kvm` on
  `ubuntu-24.04-arm` and no HVF on `macos-latest`. An aarch64 guest there would
  be TCG on an arm64 host, which is what the dev host already is.
- **A green guest suite.** Eleven names are red at a rate now known to the
  nearest fifth, and five of them reproduce. §9.1 is the measurement, §9.2 the
  list, §9.3 the one that is diagnosed. **What is left is defects in the tree,
  not work on CI** — which is the state this whole document was aimed at, and
  the reason the list can be handed to whoever owns each subsystem.

## 9.1 The rate, measured: five reps of the whole configuration

Every judgement above this section rested on two or three samples, and three runs
of one configuration had produced red lists of ten, sixteen and five names with
only three in all of them. `probe-rate.yml` is the measurement that turns that
argument into a number: **the exact shape `ci.yml` runs** — same image, same
accelerator, same `--jobs 1`, same twelve-way partition — **five times over**, all
sixty jobs, every log kept.

Run `31258202923`, tree `f8f73e1`. **All sixty jobs finished**, none cancelled and
none near the 60-minute guard; 292 tests per rep, **1460 outcomes**. Per-shard
suite seconds, one column per rep:

| shard | r1 | r2 | r3 | r4 | r5 |
|---|---|---|---|---|---|
| 1 | 401.8 | 423.0 | 423.8 | 429.5 | 428.8 |
| 2 | 214.3 | 219.9 | 222.1 | 230.8 | 195.1 |
| 3 | 308.1 | 457.3 | 429.0 | 430.7 | 431.0 |
| 4 | 407.2 | 436.7 | 386.2 | 405.7 | 328.2 |
| 5 | 343.3 | 437.3 | 436.7 | 427.2 | 432.3 |
| 6 | 305.8 | 319.4 | 291.2 | 304.8 | 287.4 |
| 7 | 502.2 | 516.1 | 504.2 | 514.4 | 507.6 |
| 8 | 349.5 | 420.1 | 400.6 | 418.1 | 334.1 |
| 9 | 466.8 | **745.7** | 457.8 | 466.4 | 484.8 |
| 10 (the 180-test shared block) | 507.4 | 415.2 | 529.8 | 510.8 | 498.7 |
| 11 | 343.8 | 331.1 | 336.6 | 347.2 | 352.1 |
| 12 | 256.7 | 202.1 | 246.4 | 236.9 | 216.7 |

The partition holds: the widest shard is 502–516 s in every rep and the narrowest
195–257 s, so the profile is doing its job. **Shard 9 rep 2 at 745.7 s against
457.8–484.8 s for its own four siblings is the runner's own variance**, on a job
whose every verdict was green — worth knowing before reading any single job's
wall clock as a finding.

**281 of the 292 names were green in all five reps.** The other eleven are §9.2.

**The tree moved between the last write-up and this one, and it took twelve names
off the list.** Run `31252989653`'s five reds were measured on `ab7f5d6`, which
does **not** contain `wt/toyos-clock`'s landing (`5b6e192`, and `1cf7fee`,
`c546335`, `02a3bc9`, `d50a8c9` under it) — the "wait N seconds, then assert what
you were waiting for" work. Run `31254054628` is the first on a tree that has it,
and every name that had been rotating is gone from both it and this probe:
`metal_sim_client_death`, `metal_sim_window_drag`, `metal_sim_pointer_churn`,
`metal_sim_compositor_stall`, `desktop_audio_client`, `desktop_typing_damage`,
`doom_sound_flood`, `i8042_health_cadence`, `sshd_fail_closed`, `xhci_hotplug`,
`xhci_hid_break` and `screen_pager_keys` are **0 of 5**.

## 9.2 The eleven, with their rates

| test | red | shard | `Sched` | what it says |
|---|---|---|---|---|
| ~~`usb_transport_break`~~ | ~~**5/5**~~ | 6 | Serial | **CLOSED** — the Bulk-Only Reset raced the transfer it recovered from (`specs/known-issues.md` §8) |
| `std_unwind` | **5/5** | 10 | shared block | `exit code Some(-1)` — a #MF, §9.3 |
| `std_unwind_so` | **5/5** | 10 | shared block | the same |
| `metal_sim_null_audio` | **5/5** | 11 | Serial | soundd did not present a null sink on a device-less machine — **closed**, see below |
| `hda_tone` | **4/5** | 4 | Serial | 1 mid-tone silence in the capture |
| `late_storage_connect` | 2/5 | 7 | Serial | the boot scan bound a disk, so the port was not held empty |
| `hda_two_live_refused` | 2/5 | 2 | Parallel | "presenting a null sink" never reached the boot console — **closed**, see below |
| `blocked_dump` | 2/5 | 3 | Parallel | two *different* reasons — the census half, and /bin/terminal racing the compositor |
| `dump_nmi_probe` | 1/5 | 2 | Serial | the rip resolved to `u128_div_rem`, not to the spin |
| `kernel_heartbeat` | 1/5 | 5 | Serial | 2 of 12 heartbeats dropped a healthy CPU from the mask |
| `usb_disk_index_stable` | 1/5 | 2 | Parallel | nothing enumerated on the first controller |

**The top five reproduce and are therefore defects, not noise.** Four of them
fail identically every time; `hda_tone` misses one rep and is
`specs/known-issues.md` §4's open item, which #88's exemption correctly does not
cover.

**Two are closed and the defect was in this file's own subject matter, not in
the guest.** `metal_sim_null_audio` and `hda_two_live_refused` red on the same
missing line, and `probe-nullsink.yml` (run `31263831141`, three reps on the
`debian:sid`/KVM image) caught the line arriving 64 ms after a 500 ms window had
closed on one rep and half a second *before* the ready marker on the other two.
soundd presents its null sink on every one of those boots; what differed is that
init spawns its programs without waiting, so the ready marker orders nothing
about a daemon's own first line — and these two were the only tests reading that
line through a span of host wall clock. Both wait on the guest now.
`specs/known-issues.md` §8 has the table and the half-second of skew between two
init children that the probe found and did not explain, which is §7's contention
class showing up somewhere new.

**Six of the eleven are `Sched::Serial`, and the harness re-ran none of them**
until this task — the retry loop was written for the parallel phase and branched
on the run's width. So half the list had no second sample at all, which is most
of why the earlier lists looked like they rotated.

**The bottom six are a rate and the rate is 20–40%, which is not "noise" either.**
The bar this work was measured against is "green means green, red means a real
defect, and a re-run tells you which"; one run in five is far above the one in
fifty that bar treats as tolerable. Each is named in `specs/known-issues.md` with
this number beside it, and none is a candidate for an `EXPECTED_FAILURES` entry —
an exemption names a defect and its write-up, and "fires 40% of the time for
reasons nobody has looked at" is not one.

## 9.3 `std_unwind` is a #MF, and the harness had been deleting the evidence

The most defect-shaped item on the list, and it took one harness fix to read.
`exit code Some(-1)` is the kernel saying it killed the process —
`recover_or_halt` answers a Ring 3 fault with `kill_process(-1)` — and everything
saying why is a `log!` line, which `run_test_paced` files under `serial` and
`check_rust_result` never printed. Three CI runs reported those eleven characters
and nothing else. With `kernel_account` in, run `31259401277` shard 10 says:

```
[kernel 8.689 cpu1 tid=0] spawn: /bin/test_rs_std_unwind pid=112 ... cr3=0x526e000
[kernel 8.711 cpu0 tid=1] !!! FAULT rip=0x0000010000097176 cr2=0x0 err=0x0 ... tid=1
[kernel 8.711 cpu0 tid=1] SIGFPE tid=1: x87 floating-point exception
[kernel 8.711 cpu0 tid=1]   rip:
[kernel 8.711 cpu0 tid=1]     unwinding::unwinder::with_context::delegate::<
                                UnwindReasonCode, _Unwind_RaiseException::{closure#0}>+0x1e6
[kernel 8.711 cpu0 tid=1]   Backtrace:
                              unwinding::unwinder::arch::x86_64::save_context
                              __rustc::rust_panic
                              ...
                              std::sys::thread::toyos::thread_trampoline
[kernel 8.716 cpu0 tid=1] exit: test_rs_std_unwind pid=112 code=-1 cpu=20ms
```

**A #MF — vector 16, no error code — inside the unwinder, on the spawned
thread.** Both binaries fail on the same sub-test: the one that panics on a
thread. The first two panics of each unwind cleanly and print `ok`. The process's
main thread was spawned on **cpu1** and the fault is on **cpu0**.

`specs/known-issues.md` §1 already records the other end of this: **no context
switch saves x87 state** — no `fxsave`, `fnsave` or `fsave` anywhere in
`kernel/src` — and `fault_gates`' `mf` arm, the only thing in this tree that
executes an x87 instruction at all, unmasks IM, computes 0/0 and expects the
`fwait` two bytes later to trap. If the kernel kills that child at the `fwait`,
the trailing `fninit` never runs and that CPU's FPU keeps IM unmasked with IE and
ES set, waiting for whatever is scheduled there next.

**`probe-x87.yml` settles it in one token.** Run `31260763462`, two arms of three
reps, one runner, one commit, one shard, and the only difference is
`fault_gate_child`'s control word:

| arm | `fault_gates` | `std_unwind` | `std_unwind_so` |
|---|---|---|---|
| `control` (`cw = 0x037E`, IM unmasked) | PASS ×3 | **FAIL ×3** | **FAIL ×3** |
| `masked` (`cw = 0x037F`) | PASS ×3 | PASS ×3 | PASS ×3 |

So the red is a kernel isolation defect that CI found and that the dev host
cannot: **any Ring 3 process can leave a pending unmasked x87 exception behind
and kill the next unrelated process scheduled on that CPU.** It belongs to
§1's entry and is not fixed here — the fix is x87 state on the context switch,
in `kernel/src/arch/`, which is a subsystem change with its own owner.

**Two repairs would turn this red green and only one of them is a fix.** Giving
`fault_gates` a boot of its own is the shape `readdir_bound` and
`abuse_short_sleep` already have, it is one line, and it would delete the only
observation of the defect this tree has. Do not take it — not before x87 state is
saved, and not after without a gate that asks the question on purpose. The same
goes for an `EXPECTED_FAILURES` entry: it would name a real defect and a real
write-up and it would still be an exemption bought to make a run green while a
process can kill its neighbour.

## 9.4 What a red CI run says now, and what a green one would

Run `31261669826` is the first on a tree carrying this task's harness work, and
it is the check that the work does what it says. Five shards red, six names:

```
guest (6)   FAIL usb_transport_break   ALONE: red again — the defect is real.
guest (11)  FAIL metal_sim_null_audio  ALONE: red again — the defect is real.
guest (8)   FAIL xhci_slow_connect     ALONE: red again — the defect is real.
guest (10)  FAIL std_unwind            ALONE: GREEN, and it was alone both times
guest (10)  FAIL std_unwind_so         ALONE: GREEN, and it was alone both times
guest (2)   FAIL usb_disk_index_stable ALONE: GREEN, and it was alone both times
guest (2)   FAIL hda_two_live_refused  ALONE: red again — the defect is real.
```

- **`usb_transport_break` and `metal_sim_null_audio` are `Sched::Serial` and both
  carry an `ALONE:` line.** Before this task neither did, because the retry loop
  read the run's width; the two most reproducible reds on CI had never been
  re-run once.
- **`shard-{2,6,8,10,11}-serial` artifacts exist**, 0.8–4.3 KB each. The step that
  claimed to keep what a red run left had uploaded nothing, ever.
- **Six names, and every one of them is on a list with a number beside it.**
  Five are §9.2's; `xhci_slow_connect` is 0 of 5 in the rate probe and is
  `specs/known-issues.md` §8's own entry — a 1 ms margin inside the guest's boot
  that reds whenever anything moves boot by ten milliseconds.

So **a red run is now readable without a second run**: the name, the sentence,
whether it survived alone, the kernel's own account if the kernel killed it, and
a rate to compare against. That is the bar this was aimed at — *green means
green, red means a real defect, and a re-run tells you which* — met on the second
and third clauses.

**The first clause is not met and cannot be by CI work.** Five names reproduce,
so a run is red every time, and each one is a defect in the tree with an owner:
soundd's device-less path (`metal_sim_null_audio`, `hda_two_live_refused`), the
xHCI transport (`usb_transport_break`), HDA's mid-tone silence (`hda_tone`, §4)
and x87 on the context switch (`std_unwind`, §9.3). Fixing those is what makes
CI green, and none of it is CI.

**One of the five is fixed, and it needed both instruments to be.** The xHCI
transport red was a driver defect the dev host is structurally unable to see: a
Bulk-Only Reset issued while the device could still answer the transfer it was
recovering from, which a TCG guest is too slow to reach in time and a KVM guest
wins every run (`specs/known-issues.md` §8). `probe-xhci-break.yml` is what
settled it — the §7.3 pattern with a **control arm that drops main's driver back
into the branch's tree**, so the reproduction and the fix are measured on one
runner in one session: control 3 of 3 red, fixed 3 of 3 green, run
`31264371902`. That control-arm shape is the reusable part; before it, every
claim about a CI-only red rested on comparing two runs taken hours apart.

### Larger runners: not purchasable for this repository

The owner approved paying for them and there is nothing to buy. GitHub's larger
hosted runners are created by name at the *organization* level and `Japabu/toyos`
is owned by a User account, so no such label resolves. Measured rather than read
off a page: run `31246336130` asked for `ubuntu-latest-4-cores`,
`ubuntu-latest-8-cores` and `ubuntu-latest-16-cores` beside an `ubuntu-24.04`
control. The control finished in **4 seconds**; the three larger jobs were still
`queued` **thirty minutes** later, which is what an unresolvable label does — it
waits rather than failing. Cancelled at that point.

What they would have bought is bounded and now mostly spent. Core count was the
standing constraint because four cores held two lanes; **one lane per machine and
twelve machines removes that**, costs nothing on a public repository, and it is
what took the `ALONE: GREEN` class from eight to zero of that shape. The one
thing a bigger machine would still buy is the six tests that boot `-smp 8` —
QEMU says so itself, `Number of SMP cpus requested (8) exceeds the recommended
cpus supported by KVM (4)` — and at one lane those now pass. If the owner moves
this repository under an organization, one 8-core shard for the wide-SMP tests is
the shape to buy, and nothing else.

## 10. Landing moved to GitHub

`cargo run -- --land` is retired. Everything reaches `main` through a pull
request, and the gate is the twelve KVM shards rather than one cross-arch TCG
run on the dev host.

The reason is §7 and §8.1, not tidiness. The dev host is arm64 emulating x86 and
gives you one vendor's reading of every instruction it emulates; `STAR[63:48]`
was green in every run there had ever been here and lost 64 boots of 64 on an
EPYC. A gate that cannot execute a class of defect is not a gate against it.

### 10.1 The merge queue is not available on this repository

GitHub's merge queue is precisely the feature that would have replaced
`--land`'s integration lock and its "gate the merged result" property in one
move: it builds a temporary branch of base + the entries ahead + this pull
request, runs the required checks on *that*, and merges only what it tested.

It cannot be turned on here. Measured, 2026-08-08, rather than read off a page:

```
$ gh api -X POST repos/Japabu/toyos/rulesets --input mq.json     # merge_queue rule
{"message":"Validation Failed","errors":["Invalid rule 'merge_queue': "],"status":"422"}

$ gh api graphql -f query='{repository(owner:"Japabu",name:"toyos"){mergeQueue(branch:"main"){id}}}'
{"data":{"repository":{"mergeQueue":null}}}
```

`MERGE_QUEUE` **is** in the `RepositoryRuleType` enum and `MergeQueueParametersInput`
has all seven of its fields, so this is an entitlement and not a missing API. A
control ruleset carrying `non_fast_forward` and nothing else was accepted on the
same repository seconds earlier and then deleted, so rulesets themselves work.

Same cause as §9.4's larger runners: `Japabu/toyos` is owned by a User account.
**If the owner ever moves this repository under an organization, the merge queue
is the first thing to buy** — it is strictly better than §10.2, because it tests
combinations without making anyone re-merge.

### 10.2 What replaces it, and why it keeps the property

**A required status check marked *strict*** — GitHub's "require branches to be
up to date before merging". The merge button stays refused until the head branch
contains `origin/main`, which means:

- **The checks that ran on the head ran on the merged result.** That is step 2 of
  the old protocol, unchanged in substance: `git merge --no-ff main`, then gate.
  It is the property that catches a semantic conflict between two branches that
  each pass alone, and losing it was the one thing a naive "CI on the PR" setup
  would have cost.
- **Landings serialise.** The first merge moves `main`; every other open pull
  request is out of date from that instant and has to merge again and re-run.
  That is the integration lock, enforced by the thing that actually moves `main`
  rather than by an advisory `flock` on one host — and §5 already recorded two
  landings that got past that lock because nothing outside the build system could
  take it.

What it costs against a merge queue is parallelism: two ready pull requests are
merged one after the other with a full CI run between them, where a queue would
have built them speculatively. On a repository with one owner and a handful of
agents that is a queue of two, and the minutes are unmetered.

`cargo run -- --pr` is the command that produces the state the button wants:
preflight refusals, `git fetch`, this host's `main` fast-forwarded,
`git merge --no-ff origin/main` into the branch, `git push -u`, and the `gh`
lines to run next. It never pushes `main` and never forces anything.

### 10.3 Where each of `--land`'s invariants went

| invariant | where it lives now |
|---|---|
| integration lock, one landing at a time | GitHub, via strict required checks (§10.2). `buildlock::integration` survives with a narrower job: one process at a time moves *this host's* `main`, which is a tree somebody may be building in. |
| `git merge --no-ff main`, gate the merged result | `cargo run -- --pr` makes the merge; the strict check refuses the button without it. |
| `--ff-only` into main | GitHub's merge. It is still a merge commit with both parents and nothing is squashed or rebased — `allow_squash_merge` and `allow_rebase_merge` are off. |
| the ABI-first refusal | `landing.yml`'s `abi-split` check, and `cargo run -- --pr` locally. One function (`pr::abi_lands_alone`) answers both. |
| `--abi-inseparable` | an `Abi-Inseparable: <why>` trailer in a commit message. CI has no command line from the author, and a flag's only record was the commit `--land` wrote; a trailer is in the branch's history and lands with it. |
| the landing commit's message | the pull request's title and body, through `merge_commit_title=PR_TITLE` / `merge_commit_message=PR_BODY`. `.github/pull_request_template.md` asks for what `--land` used to compose. |
| "gate ok" vs "gate ok, NOT clean" | two places, and both are better than a sentence in a commit body. The declaration itself is `EXPECTED_FAILURES` in `tests/toyos.rs`, which is *in the diff being reviewed*; and every shard now writes its own `test result:` line into the run's job summary. |
| which gate ran, and how long | the check run on the head commit, which is durable and linked from the merge. |
| the sysroot claim and its standing rule | **local, and it stays local.** It is about one shared 50 GiB `rust/build` on one laptop; a runner has its own and §8.1 says so. Nothing about it is expressible on GitHub. |
| `origin/main` falling behind | gone. `main` moves only through a merge, so GitHub is where it moves. |
| fast-forwarding this host's `main` | `cargo run -- --sync`, which `--pr` runs first. The primary checkout still owns `rust/`, the rustup link and the witness every worktree compares against, so it has to keep up. |

### 10.4 The two stages, and the exact trigger between them

**CI cannot currently go green, and the reason is not CI.** §9.2 measured five
names reproducing on every run. Three closed on 2026-08-08 —
`usb_transport_break` a real xHCI driver defect only CI could see,
`metal_sim_null_audio` and `hda_two_live_refused` two harness waits taken over a
span of host wall clock — and **three remain**: `std_unwind` and `std_unwind_so`
(x87 state on the context switch, §9.3) and `hda_tone`'s mid-tone silence
(known-issues §4). Each has an owner and a write-up. **Widening
`EXPECTED_FAILURES` to cover them is refused** — an exemption names a defect and
its write-up, and buying a green run while any Ring 3 process can kill the next
unrelated one scheduled on that CPU is not that.

So the switch is staged, and the second stage is not a matter of anyone
remembering.

**Stage 1, now.** `main` is protected: pull request required, force-push and
deletion refused, no bypass. The required status checks are

```
host        every host crate, cargo test --lib, sshd          (host-tests.yml)
abi-split   the ABI-first rule                                (landing.yml)
gate-stage  whether the guest gate is still owed              (landing.yml)
tcg         one guest boot, emulated, on x86_64               (ci.yml)
```

`guest-suite` — the twelve KVM shards behind one name — runs on every pull
request and is **not** required.

**Stage 2, on the trigger.** Add `guest-suite` to the required checks and set
`GUEST_GATE: required` in `.github/workflows/landing.yml`.

**The trigger is three consecutive completed `ci` runs on `main`, all green.**
Three and not one, because six of §9.2's eleven fire at 20–40% and one lucky
green is a sample rather than a state.

**`gate-stage` is the check that makes the trigger fire by itself.** It reads
this repository's own last three completed `ci` runs on `main` and goes red when
they are all green while `GUEST_GATE` still says `advisory`, printing both halves
of the remedy. Nothing then merges until the gate is promoted. That is
`EXPECTED_FAILURES`'s `OnAPass` applied to the gate's configuration: an advisory
state that cannot survive being unnecessary.

### 10.5 What is configured outside the repository

None of this is in VCS and none of it is reviewable in a diff, so it is listed
here. Repository settings on `Japabu/toyos`:

- **Ruleset `main`** — `pull_request` (0 approving reviews required),
  `required_status_checks` with `strict_required_status_checks_policy: true` and
  the four contexts of §10.4, `non_fast_forward`, `deletion`. No bypass actors.
- `allow_squash_merge: false`, `allow_rebase_merge: false` — nothing rewrites
  history, which is CLAUDE.md's rule said at the remote.
- `allow_auto_merge: true` — `gh pr merge --auto --merge` is how an agent leaves
  a pull request to merge itself when the checks come back.
- `merge_commit_title: PR_TITLE`, `merge_commit_message: PR_BODY` — the merge
  commit is the landing record, so it reads from main's side the way `--land`'s
  did.
- `delete_branch_on_merge` stays **off**: a branch is checked out in a worktree
  and deleting it under one is not a tidy-up.

**Zero approving reviews is deliberate and is the one place this is weaker than
it looks.** There is one human and several agents; requiring a review would
deadlock every agent on the owner being awake. The review gate `specs/worktrees.md`
§5 argues for — changes to the files that govern other agents — is a thing the
owner does on the pull request, and a pull request is the first artifact this
workflow has ever had that he *can* do it on.
