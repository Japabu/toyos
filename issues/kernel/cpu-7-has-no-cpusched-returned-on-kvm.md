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
wrote it back to `None` — which is the class
`issues/kernel/a-btreemap-panicked-inside-its-own-insert-in-a-scheduler-pass.md`
tracks, and whose current suspect is the kernel heap rather than the scheduler.

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

## Whoever takes it

Read `driver.rs:429`'s fill against the AP release path first — it is a
one-reader question and it either holds or it does not, which is worth settling
before spending storms on the heap hypothesis. If it holds, this joins the heap
class and the KVM host is the new instrument for it.
