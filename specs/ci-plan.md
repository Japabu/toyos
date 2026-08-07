# Continuous integration on GitHub

Everything here was measured on GitHub-hosted runners against `Japabu/toyos`
(public, so Actions minutes are free and unmetered). Run ids are given so every
number can be re-read.

## 1. What a runner is

Measured 2026-08-07, run `31192565891`, five labels in one matrix.

| label | CPU | cores | RAM | free disk | `/dev/kvm` | QEMU from apt/brew |
|---|---|---|---|---|---|---|
| `ubuntu-latest` (24.04) | AMD EPYC 9V74 | 4 | 15 GiB | 88 GB | present | 8.2.2 |
| `ubuntu-24.04` | AMD EPYC 7763 | 4 | 15 GiB | 88 GB | present | 8.2.2 |
| `ubuntu-22.04` | Xeon Platinum 8370C | 4 | 15 GiB | 87 GB | present | 6.2.0 |
| `ubuntu-24.04-arm` | (arm64) | 4 | 15 GiB | 109 GB | **absent** | 8.2.2 |
| `macos-latest` (arm64) | — | — | — | 97 GiB | absent | 11.0.3 (brew) |

Installing QEMU costs 17–48 s from apt and 22 s from brew, so it is not worth
caching.

`ubuntu-22.04`'s QEMU 6.2 predates `virtio-sound-pci` (QEMU 8.2), which every
audio boot in this tree uses. It is out.

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

So the answer to the question that decides the whole design is **yes on
x86_64**, and **no on ARM** — `ubuntu-24.04-arm` has no `/dev/kvm` node at all,
and `macos-latest` reports no `kern.hv_support` and aborts on `-accel hvf`.
An aarch64 guest running natively is therefore not available today, which only
matters once ToyOS has an ARM64 port.

`kvm_amd` and `kvm_intel` are both loaded depending on which host the job lands
on, so a run can be scheduled onto either vendor. The host CPUID as seen from
`/proc/cpuinfo` offers `constant_tsc nonstop_tsc pcid rdtscp smap smep xsave
avx2` and does **not** list `x2apic`, `tsc_deadline_timer` or `invtsc`.

## 3. Open — measured next

- The toolchain on a runner: whether `cargo run -- --build-only` completes
  there at all, and what it costs. Probe running.
- The suite under KVM against the same suite under TCG, on the same runner.
- Gate A's spread across N runs on a runner, against the spread
  `tests/audio-baseline.toml` records for the owner's host.
