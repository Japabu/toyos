---
status: open
kind: defect
opened: 2026-08-21
---

# `cpu 7 has no CpuSched` came back, on x86 hardware under KVM, from the idle loop

`src/redlist.rs`'s retired `sched_stress` row closes with "a red under this name
now is a new measurement". This is one, and it is the first sighting of this
signature that is **not** the dev host under cross-arch TCG.

2026-08-21, tree `53101d08` (`main`), on the self-hosted T14 — Intel i5-1135G7,
KVM, the CI image at the digest `route.yml` names, QEMU 11.1.0 — during an
`--audio-gate 2` warm-up run, on the `audio_tone smp=8` boot of iteration 2. The
harness read it as `Init process crashed during boot` and exited 101.

```
[kernel 0.592 cpu7] PANIC: panicked at src/sched/driver.rs:224:28:
cpu 7 has no CpuSched
[kernel 0.592 cpu7]   Backtrace:
[kernel 0.593 cpu5] CPU 5: joining scheduler
[kernel 0.593 cpu5] sched: cpu=5 ready=0 dying=0 parked=0 current=None trips=1
[kernel 0.593 cpu2] CPU 2: joining scheduler
[kernel 0.593 cpu2] sched: cpu=2 ready=0 dying=0 parked=0 current=None trips=1
[kernel 0.593 cpu7]     0xffff80007cb53d2c  core::panicking::panic_fmt+0x2c
[kernel 0.593 cpu7]     0xffff80007ca42223  kernel::sched::driver::with_cpu::<toyos_sched::cpu::Action<kernel::sched::payload::KernelPayload>, kernel::sched::driver::pass::{closure#0}>+0x283
[kernel 0.593 cpu7]     0xffff80007ca3cf24  kernel::sched::driver::pass+0x94
[kernel 0.593 cpu7]     0xffff80007ca3d866  kernel::sched::driver::idle_loop+0x26
[kernel 0.593 cpu7]   Contexts: cpu7 crashed at rsp=0xffff800000f07d98, asking about ctx 0x0
[kernel 0.593 cpu7]   cpu0 is on ctx 0x0 (never switched, or not a context)
[kernel 0.593 cpu7]   cpu1 is on ctx 0xffff800001b56748 pid=1 tid=0 stack_top=0xffff800001bca000 saved_rsp=0xffff800001bc9fc0
[kernel 0.593 cpu7]   cpu2 is on ctx 0xffff800001900310 (its idle context) stack_top=0x0000000000000000 saved_rsp=0xffff800000e62ef0
[kernel 0.593 cpu7]   cpu3 is on ctx 0xffff8000019003a0 (its idle context) stack_top=0x0000000000000000 saved_rsp=0xffff800000e83ef0
[kernel 0.593 cpu7]   cpu4 is on ctx 0x0 (never switched, or not a context)
[kernel 0.593 cpu7]   cpu5 is on ctx 0x0 (never switched, or not a context)
[kernel 0.593 cpu7]   cpu6 is on ctx 0x0 (never switched, or not a context)
```

## What is new about it

**It is `idle_loop`, during AP bring-up, not a pass that had already run.** The
prior two sightings the retired redlist row describes were both "after cpu7 had
already completed a pass"; this one is cpu7's arrival at
`driver::idle_loop -> pass -> with_cpu`, at 592 ms, with cpu2 and cpu5 still
printing `joining scheduler` one millisecond later and cpu4/5/6 never having
switched. `driver.rs:212` states the invariant this contradicts in as many
words: *"`init` fills `SCHEDS` before any AP is released"*. Either that ordering
does not hold on the KVM bring-up path, or the slot was filled and something
wrote it back to `None`.

**And the instrument is new.** Every previous measurement of this class is the
dev host under cross-arch TCG; `tests/CLAUDE.md` warns that TCG is one vendor's
reading of the ISA and that a real accelerator is only exercised by CI's KVM
shards. This is real x86 hardware with a real accelerator, so the class is not
an artifact of emulation.

## Rate, and the honest caveat about it

One death in 254 boots this session, and **the only death was on a contended
host**: the warm-up it happened in shared the T14 with a live CI job container
(witness: `ci=1` throughout, 1-minute load 3.5-4.8 on 8 threads with an 8-vCPU
guest). The 248 boots taken afterwards on an idle machine — the four interleaved
gate A blocks, 240 boots, plus an 8-boot warm-up — produced none.

Under CLAUDE.md's 2026-08-04 ruling that is **not** an excuse and not grounds to
re-run it away: an oversubscribed host is a scheduling shape the kernel has to
survive, and a bring-up ordering bug is exactly the kind of thing that only
shows when the vCPUs are not all running at once. What the load buys is a
*reproducer*: the existing recipe for this class is a wide boot storm, and
running one on a deliberately oversubscribed KVM host is a cheaper instrument
than 6,576 TCG boots per sighting.

## 2026-08-21: the stray-write class this was read as a sighting of is resolved

The dev host's `BTreeMap`-inside-its-own-`insert` class — a per-CPU scheduler
record reading as a value no operation on it produces — was resolved that day:
no Ring 0 entry cleared the direction flag, `compiler_builtins::mem::memmove`
sets it across three `rep` string operations with interrupts enabled, and every
`memcpy`/`memset` reached from an entry taken inside that window wrote the `n`
bytes *below* its destination. `arch::entry::ring3_naked_asm`'s `cld` and
`arch::syscall::init`'s `DF` in `IA32_FMASK` close it: 17 deaths in 7,059
twelve-wide boots without them, 0 in 7,418 with them.

`SCHEDS` is `static` and a `None` written back into it is exactly what a
backwards `memset`/`memcpy` landing in `.bss` produces, so **this sighting is
consistent with that mechanism and needs no bring-up ordering bug at all** — the
fix is architecture-neutral and applies to the KVM path unchanged. What is owed
is a re-measurement, not a new hypothesis.

## A second sighting the same day, on the dev host, on the newer tree

2026-08-21, `wt/toyos-bimodal` at `fa5eb83b` (`main` `13953023` plus a soundd
stats-line instrument), dev host, cross-arch TCG, plain `cargo test` — the
12-wide fast tier. One death in 275 tests:

```
[kernel 1.366 cpu1] PANIC: panicked at src/sched/driver.rs:264:28:
cpu 1 has no CpuSched
[kernel 1.367 cpu1]     0xffff80007d15419c  core::panicking::panic_fmt+0x2c
[kernel 1.367 cpu1]     0xffff80007d122f03  kernel::sched::driver::with_cpu::<...>+0x283
[kernel 1.367 cpu1]     0xffff80007d08c234  kernel::sched::driver::pass+0x94
[kernel 1.367 cpu1]     0xffff80007d08cb76  kernel::sched::driver::idle_loop+0x26
[kernel 1.367 cpu1]   Contexts: cpu1 crashed at rsp=0xffff800000e41d98, asking about ctx 0x0
[kernel 1.367 cpu1]   cpu0 is on ctx 0xffff800000debd48 pid=0 tid=0 ...
```

Byte-for-byte the same shape — `idle_loop -> pass -> with_cpu`, `asking about
ctx 0x0`, during bring-up — at the same site: `driver.rs:264` is the `:224`
above after `main` grew `kernel/src/sched/driver.rs` by 157 lines and
`kernel/src/mm/alloc.rs` by 651 between `53101d08` and `13953023`. So **the heap
sweep and the scheduler work that landed between the two sightings did not
remove it**, and it is not specific to KVM, to eight vCPUs or to the audio
configs: this one was a four-CPU boot on a host running twelve guests at once.

The red it produced was named `swiss_german_layout`, which was only the test
whose boot it landed in — `tests/CLAUDE.md`'s "that red's name is the workload,
never the cause". The harness's own `ALONE:` line then reported it GREEN alone
and blamed the test's `Sched::Parallel`, which is the misreading the same file
warns about two lines later. Anyone meeting a lone `Init process crashed during
boot` under an unrelated test name should search the capture for this panic
before believing the name.

## 2026-08-22: the re-measurement, on this machine, and what it will not do

The recipe above was run on the T14 itself: the CI image at the digest
`route.yml` names, `--device=/dev/kvm`, QEMU 11.1.0, `-cpu host,+rdrand,+smap,
+fsgsbase,+x2apic,+smep`, `-machine q35,kernel-irqchip=split` with the IOMMU,
`-smp cores=8`, `-m 2G`, the boot stick and the NVMe disk both `snapshot=on`,
**six guests at once — 48 vCPUs on eight threads**, which is a harder
oversubscription than the sighting's own (one 8-vCPU guest beside a CI job).
`compositor: ready` is the completion marker and never a timer; every guest ran
with `-action reboot=shutdown -action shutdown=pause -action panic=pause`, so a
death that says nothing parks and has its vCPUs read over QMP rather than being
lost. Blocks were 240 s, alternated ABBA, and any block that shared the machine
with a CI job container was discarded and re-run — one was, and no accepted
block has a witness sample showing company.

| arm | kernel | boots | deaths | silent | hangs | `has no CpuSched` |
|---|---|---|---|---|---|---|
| U | shipped, `cld` reverted (`entry-df-unclean`) | 1,790 | **0** | 0 | 0 | 0 |
| F | shipped | 1,789 | **0** | 0 | 0 | 0 |
| UA | `sched-tripwire stack-witness`, `cld` reverted | 7,186 | **0** | 0 | 0 | 0 |
| FA | `sched-tripwire stack-witness` | 6,790 | **0** | 0 | 0 | 0 |

17,555 boots, no death of any kind, no hang, and no QEMU exit. `UA`/`FA` are the
dev host's own arms A and C carried onto this machine, feature for feature; `U`
and `F` carry no instrument at all, so they are the kernel this repository
ships. `objdump` of the four kernels puts `cld` at offset 0 of `common_entry`,
`syscall_entry`, `timer_entry`, `tlb_flush_entry` and all six
`device_irq_entry!` expansions in `F`/`FA` and at none of them in `U`/`UA`, with
`IA32_FMASK` `$0x40600` against `$0x40200`; each guest's boot line names the
size of the kernel it loaded, and the four sizes are the four disassembled
files.

**The signature did not return, and the bring-up-ordering hypothesis is refused
at this width.** This file's own rate — one death in 254 boots — carried onto
the 8,579 fixed boots expects 33.78 and observed none (Poisson, p = 2.1e-15);
onto all 17,555 it expects 69.11 (p = 9.6e-31). `driver.rs:212`'s "`init` fills
`SCHEDS` before any AP is released" is not violated at any rate this machine can
be made to show.

**And neither did anything else, which is the half that has to be said.** The
unfixed arms stage the whole defect and produced no death either. So this run
*bounds* the class on real silicon; it does not attribute the sighting to
anything. Carried the other way, the dev host's cross-arch TCG rates are refused
here: arm A's 17 in 7,059 expects 17.31 in `UA`'s 7,186 (p = 3.0e-8), and the
pooled unfixed 37 in 13,960 expects 23.79 in the 8,976 unfixed boots taken here
(p = 4.7e-11).

The likely reason is time rather than architecture. The window is
`compiler_builtins::mem::memmove`'s `std` … `cld` and what has to land in it is
an interrupt, so exposure per boot scales with timer ticks per boot — and a TCG
boot on the dev host spends many times the wall clock a KVM boot spends on the
same guest work. Nothing about the mechanism is emulator-specific; the *rate*
is, and no storm on this machine has yet been made dense enough to see it.

The second sighting recorded above is the control for that reading rather than
against it: it is the dev host again, cross-arch TCG again, and a **four**-CPU
boot, so neither eight vCPUs nor KVM is what this class needs. Every sighting
this project has on an unfixed tree but one came from the slower instrument, and
the one that did not is the 254-boot session this file opened with.

## Whoever takes it

Do not re-run this recipe wider: it has been run at twenty times this sighting's
width, on this sighting's machine, under heavier oversubscription, and neither
arm speaks. What reopens the bring-up question is a *new* sighting of `cpu N has
no CpuSched` on a kernel carrying the `cld`, and `driver.rs`'s fill against the
AP release path is still the one-reader thing to read first if one arrives.
What would settle the rate question instead of waiting on it is a reader at the
entry itself — the flag as the kernel finds it, before `arch::entry`'s `cld`
clears it — because that counts exposure rather than the fraction of it that
turns fatal.
