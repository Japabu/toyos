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

§7 is closed. The guest shards still run TCG, because that is the wide
repeatable measurement and the one comparable with the dev host's; the KVM job
beside them is the only gate that *executes* `syscall`/`sysret`/`iret` rather
than emulating them.

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

## 5. What runs, on what trigger

| workflow | trigger | runner | what it is |
|---|---|---|---|
| `host-tests.yml` | every push, every PR | `macos-latest` | `cargo test --lib` plus the nine host crates and `userland/sshd`. No toolchain, no guest. |
| `toolchain.yml` | every push | `ubuntu-24.04` | Publishes the content-addressed toolchain release, or does nothing. |
| `ci.yml` | every push, every PR | `ubuntu-24.04` ×5 | Four guest shards under TCG, plus one KVM canary. |
| `gate-a.yml` | `workflow_dispatch` | `ubuntu-24.04` ×2 | The thorough audio tier, one audio test per machine. |
| `probe-*.yml` | push to `ci/probe-*` | various | The measurement workflows this document is made of. |

`ci/**` is a throwaway namespace excluded from the first three, so a probe push
does not start a suite.

**macOS for the host tests is not incidental.** Two of those gates have a
macOS-only outside judge: `toyos-fat32` formats real volumes through
`newfs_msdos` and `hdiutil`, and both it and `src/image.rs` are judged by
`fsck_msdos`. There is no Linux equivalent whose output either parser
understands — which is also the one hole in the guest suite on Linux (§6).

Measured, `macos-latest`, run `31194966771`, cold: 594 s end to end — `cargo
test --lib` 183 s, the nine host crates 274 s, sshd 124 s, all of it
compilation. With the cargo cache: 442 s (run `31201564400`).

`toolchain.yml` when the release exists: **6 seconds** (run `31201563879`).

## 6. What a fresh clone does not have

**The SoundFont, and it is the one thing standing between CI and a green guest
suite.** `assets/timgm6mb.sf2` is `.gitignore` line 3 — 5,994,284 bytes doom
synthesises its music from, deliberately not carried in git. Three test configs
declare it in `untracked-assets` (`desktopcase`, `desktopaudiocase`,
`metalcase`) and a build that cannot find a declared asset is a hard error by
design, so that a fresh clone is *told* rather than handed a doom that plays
nothing (`specs/boot-image-split.md` §5). **A runner is a fresh clone**, so the
whole desktop and metal-sim families red on it.

Whether this repository may publish that file is a licensing decision and the
owner's, not CI's. `ci.yml` therefore reads a repository variable
`TOYOS_SOUNDFONT_URL`: set it and the guest job fetches the file before
building; leave it unset and the job says so in one line and lets those tests
red by name rather than mysteriously. **Nothing else is blocking a green guest
suite.** Gate A is unaffected — it runs on `tests/testcases`, which declares no
untracked asset.

Observed on the dev host too, and it is the same defect wearing worktree
clothes: `cargo run -- --worktree add` carries `.cargo/config.toml` and not
this, so a brand-new worktree fails `metal_sim_compositor` and
`metal_sim_pointer_churn` on it until somebody copies it across.

## 6.1 What does not survive a Linux host

`fsck_complaints` (`tests/common/volumes.rs`) shells out to `/sbin/fsck_msdos`
and returns `no /sbin/fsck_msdos: this gate's outside judge is missing`
anywhere else — a red, correctly, rather than a silent pass. Five guest tests
reach it: `esp_filesystem`, `kernel_log_file`, `log_partition_layout`,
`log_partition_identity`, and the toybox `cp` case. `fsck.vfat` from dosfstools
is the Linux equivalent and prints a different format, so a second arm in that
parser is real work and is **not done**.

That is the whole list. `ps -Ao comm=` and `getloadavg` — the other two host
calls the harness makes — are both Linux-native.

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

So **TCG on a standard runner is not a working configuration for this suite**,
and the shards as landed are red on it. Three directions, none taken here:

- **Run the shards on KVM.** A native guest is fast enough that none of these
  ceilings is marginal, and §7 — the reason they were on TCG at all — closed
  while this was being measured. §2 argues the other way, that TCG keeps the
  shards comparable with the dev host's numbers; that argument was written
  before this measurement existed, and the two now have to be weighed against
  each other. **This is the open decision**, and it is the one that decides
  whether the guest suite can be green at all.
- `--jobs 1` or `2` — fewer guests contending for four cores, at the cost of
  wall clock a shard already does not have.
- Scale the ceilings by a measured host speed rather than by width, which is a
  change to `qemu::budget` and to what every ceiling in the tree means.

Whichever wins, the comparison §2 wants is still available: one shard on each
accelerator costs one extra runner and nothing else.

## 8. What CI buys that the dev host cannot

- **A machine that can run these guests natively.** The dev
  host is arm64 and every guest on it is cross-arch TCG, whose distortion
  CLAUDE.md records at 1.06×–6.5× and non-uniform; the "every component within
  2× of a production OS" bar is unmeasurable there.
- **A second architecture of *host*.** Even under TCG, x86-on-x86 exercises code
  paths — `syscall`/`sysret`, segmentation, the APIC — that cross-arch TCG
  reimplements. §7 was found this way before KVM was even reached.
- **No shared sysroot.** Every runner has its own, keyed on the branch's own
  four trees, so the contention `specs/worktrees.md` §3.1–§3.2 describes — a
  claim that refuses every other worktree, measured at 35 and 50 minutes of
  nobody being able to build — does not exist there. Observed while writing
  this: the local guest suite could not be run at all for the length of another
  agent's ABI change.
- **A quiet host for gate A.** Each shard is its own VM, so
  `live_instances() == 0` holds by construction rather than by the suite
  arranging it, and no other agent's build is on that machine. The thorough tier
  is N boots per config strictly one at a time, so a second machine is the only
  thing that can shorten it: `--shard 1/2` and `2/2` put one audio test on each.

## 8.1 Somebody has to push `main`

`cargo run -- --land` fast-forwards the primary checkout and does not push, and
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
would still move only through `--land`. **Not done here** — it is a change to
the landing protocol and to the owner's rule, and both are his. Until it is, a
`main` that has stopped moving on GitHub is the thing to check first when CI
looks stale.

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
- **`fsck.vfat`**, §6.
- **A green guest suite.** §7.1 — the shards are red today, on timeouts rather
  than on assertions, and the open decision is the accelerator they run on.
- **Larger runners** were not tried. They are a billed feature even on public
  repos, so trying one is the owner's call — and §7.1 makes core count the first
  thing worth trying if KVM stays out of reach.
