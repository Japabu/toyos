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
about 30 GB in 21 s, which the toolchain build takes and nothing else needs.

`ubuntu-22.04`'s QEMU 6.2 predates `virtio-sound-pci` (QEMU 8.2), which every
audio boot in this tree uses. It is out.

**Disk is not the constraint the plan assumed.** 88 GB free on a standard
runner against the 14 GB the task brief expected, and `rust/build` is 46 GB on
the dev host with incremental on. A full bootstrap fits.

## 2. KVM: yes, after one `chmod`

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

So the answer to the question that decides the design is **yes on x86_64**, and
**no anywhere else**: `ubuntu-24.04-arm` has no `/dev/kvm` node at all, and
`macos-latest` reports no `kern.hv_support` and aborts on `-accel hvf`. An
aarch64 guest running natively is not available today, which only matters once
ToyOS has an ARM64 port.

`kvm_amd` and `kvm_intel` are both loaded depending on which host a job lands
on, so a run can be scheduled onto either vendor. The host CPUID as read from
`/proc/cpuinfo` offers `constant_tsc nonstop_tsc pcid rdtscp smap smep xsave
avx2` and does **not** list `x2apic`, `tsc_deadline_timer` or `invtsc`; the
harness already asks for `-cpu host,+rdrand,+smap,+fsgsbase,+x2apic,+smep`, so
KVM's in-kernel APIC supplies the one the kernel needs.

**And the tree had a defect this uncovered.** Both KVM checks
(`src/qemu.rs`, `tests/common/qemu.rs`) asked `Path::new("/dev/kvm").exists()`.
Presence is not permission and the two are indistinguishable from `exists`, so
on an unmodified runner — and on any Linux box whose user is not in the `kvm`
group — every boot would take `-accel kvm` and die. `toyos_build::kvm_usable`
opens the device instead, and both callers use it.

## 3. The toolchain

The sysroot is host-specific: `rust/build/<host>/stage2` is 790 MB of
`aarch64-apple-darwin` on the dev host and useless on a Linux runner, so a
runner needs an `x86_64-unknown-linux-gnu`-hosted one. Its product is that plus
`rust/build/x86_64-unknown-toyos/stage2` (363 MB), the ToyOS-hosted rustc that
`system.toml`'s `hosted-rustc = true` puts in the initrd.

Two things had to be true and both were measured rather than assumed.

**Bootstrap does not build LLVM.** `write_config` writes `profile = "compiler"`,
whose defaults include `download-ci-llvm = true`, and the dev host's
`ci-llvm` directory is 493 MB of downloaded artifact. So the cost is rustc, not
LLVM.

**But `download-ci-llvm` fails on a runner for a reason that is about forks.**
Run `31193469406`:

```
downloading https://ci-artifacts.rust-lang.org/rustc-builds/4fe0ba68111c.../rust-dev-nightly-x86_64-unknown-linux-gnu.tar.xz
curl: (22) The requested URL returned error: 404
ERROR: failed to download llvm from ci
```

`4fe0ba68111c` is *our* "Merge upstream rust-lang/rust main" commit, which
rust-lang CI never built. Bootstrap picked it because `GITHUB_ACTIONS=true`
puts `check_path_modifications` on its own CI branch, which assumes HEAD is a
PR merge commit and takes `HEAD^1` as the most recently merged upstream commit
(`src/build_helper/src/git.rs`, `get_closest_upstream_commit`). Off CI it walks
back to the newest commit authored by bors, which is a real upstream commit and
has a prebuilt LLVM. **`env -u GITHUB_ACTIONS -u CI` is the whole fix**, and it
makes the runner do exactly what the dev host does.

The submodule clone is full-history for the same reason — 106 s, and a shallow
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

One hour, once per (rust fork, `toyos-abi`, `toyos`, `libc`) — and the job
that pays it publishes for everybody.

**`Owner::Installed`** (`src/toolchain.rs`) is the third answer to "who owns the
toolchain": a checkout whose `rust/build` holds one and whose `rust/` holds no
source to have built it from. Read off the disk rather than declared, because
such a checkout has exactly one thing it can do — check that the toolchain is
the one these sources need — and a flag saying so could disagree with what is
there. It checks the same three things a linked worktree does (stage2 present,
the rustup link pointing at it, the witness matching) and refuses with "publish
a toolchain built from these sources" instead of offering a claim, because
there is nothing here to claim with.

## 4. Sharding

`testargs::Shard` is `--shard <index>/<count>`, applied to the parallel tasks,
the serial tail and gate A's configs after the phases are decided.
Longest-processing-time over the same measured duration profile
`longest_first` already orders the queue by.

**A shard is a host, not a lane.** `--jobs` divides one machine's cores between
guests that contend for them, and `buildlock::guest_slot` exists because two
suites on one host mismeasure each other. A shard divides work between machines
that share nothing, so the two compose and `--host-slots 0` is right on a
runner: there is one suite on that machine and nothing to arbitrate.

Four gates in `src/testargs.rs`, the load-bearing one being that the shards are
a partition — nothing dropped by all of them, nothing run by two — because a
shard that silently owned no tests would report the run green.

## 5. Open — measured next

- Toolchain wall clock on a runner. Probe running.
- The suite under KVM against the same suite under TCG.
- Which tests do not survive a Linux host. Known already:
  `fsck_complaints` (`tests/common/volumes.rs`) shells out to
  `/sbin/fsck_msdos` and returns "this gate's outside judge is missing"
  anywhere else, which is five guest tests. `fsck.vfat` is the Linux
  equivalent and its output is a different format.
- Gate A's spread across N runs on a runner, against the spread
  `tests/audio-baseline.toml` records for the dev host.

## 6. What CI buys that the dev host cannot

Recorded here as it is observed rather than argued in advance.

- **x86_64 with KVM.** The dev host is arm64 and every guest runs under
  cross-arch TCG, whose distortion CLAUDE.md records at 1.06×–6.5× and
  non-uniform. The "every component within 2× of a production OS" bar is
  unmeasurable in a TCG guest and is not unmeasurable here.
- **No shared sysroot.** Every runner has its own, keyed on the branch's own
  `toyos-abi`/`toyos`/`libc`, so the contention `specs/worktrees.md` §3.1–§3.2
  describes — a claim that refuses every other worktree, measured at 35 and 50
  minutes of nobody being able to build — does not exist there. Observed while
  writing this: the local guest suite could not be run at all for the length of
  another agent's ABI change.
- **A quiet host for gate A.** Each shard is its own VM, so gate A's
  `live_instances() == 0` precondition holds by construction rather than by the
  suite arranging it, and no other agent's build is on that machine.
