---
status: closed
kind: finding
opened: 2026-08-15
closed: 2026-08-17
---

# Invariant P's 200 µs budget cannot be met on the dev host, so `sched_check_build` is a CI-only gate

> **Closed, and the title is what closed.** Invariant P is gone; the budget is
> measured and gated on a distribution instead. On a quiet dev host, alone,
> `sched_check_build` now passes — the emulator meets the budget for nine passes
> in ten, and what did not meet it was the emulator on a host running eleven
> other guests. The last section is the measurement and what it cost.

`sched_check_build` boots the `sched-check` kernel — the first thing in the tree
ever to do so — and it is **green on CI and red on the dev host**, for the
instrument and not for the kernel.

On CI's KVM shards, twelve of twelve green, `sched_check_build` measured
5,879 ms (run 31875856466, PR #76). On the dev host it panics during boot, in
the idle loop, before userland:

```
[kernel 0.241 cpu0] PANIC: toyos-sched/src/cpu.rs:1094
invariant P: a scheduler pass took 1684167 ns, budget 200000 ns
  <toyos_sched::cpu::SchedPass<..>>::finish+0xa33
  kernel::sched::driver::with_cpu::<..>+0x180
  kernel::sched::driver::pass+0x94
  kernel::sched::driver::idle_loop+0x32
```

Two boots of unmodified `main`, both fired: 1,684,167 ns on cpu0 in the parallel
phase, then 1,749,243 ns on cpu1 in the isolated re-run. Eight to nine times the
200 µs budget.

**The dev host is cross-arch TCG on arm64** (`src/redlist.rs`'s
`Instrument::DevHostAlone`), emulating x86-64 instruction by instruction while
the guest TSC advances with host wall clock. A scheduler pass that fits 200 µs
natively cannot fit it there, and no amount of tuning changes that. CI is KVM on
native x86-64 with `-cpu host`, which is the instrument the budget was derived
for.

**Two other explanations were tested and ruled out**, so this is not an
assumption about TCG standing in for an investigation:

- **Not `check_cpu`'s observer cost.** That walk runs *inside* the measured
  window and exists only in this build, so it was removed outright and the pass
  re-measured: **1,705,987 ns**, against 1,684,167 with it. It accounts for
  about 20 µs of 1.7 ms.
- **Not the driver prologue.** `sched::driver::pass` samples `now` *after*
  `drain_irqs` and `drain_zero_handles`, so the xHCI prologue that
  `scheduler-pass-blocks-in-xhci.md` is about sits outside this window — exactly
  as that issue states. What is over budget is the inside of the window:
  `SchedPass::begin`'s drain, `pick`, `answer_steal_requests`, `apply_timer`.

**What is owed.** Nothing in the kernel, on this evidence. What is owed is that
an agent running `cargo test` on the dev host must not read this red as theirs,
which is what the `src/redlist.rs` row pointing here is for.

**What would change the answer.** `MAX_PASS_NS` is a policy number and its own
doc forbids the obvious response — "if it ever fires on honest work, the honest
response is to find out which pass grew and why, not to raise it". That still
holds: if invariant P ever fires on a **KVM** shard, this file does not cover it
and it is a real finding about the scheduler. The row is scoped to
`Instrument::DevHostAlone` precisely so that a CI red under this name cannot be
silenced by it.

**That has now happened, so read the two sentences above as spent.** Invariant P
fired on KVM shards twice on 2026-08-16 — 277260 ns on cpu1 in
`driver::idle_loop` (run 31936533470, a push to `main`) and 200569 ns on cpu0 in
`timer_handler` (run 31946183485) — at a measured 2 of 91 sampled `ci` runs.
`issues/kernel/the-check-build-guest-stopped-answering-on-kvm-twice.md`
carries it.

**Magnitude does not disturb this file; two other things do.** 1.68 ms is five
times the largest KVM firing, and nothing on KVM has come near it, so the TCG
explanation of *that* gap stands. But the **call site** no longer separates the
accelerators — `driver::idle_loop` is where the 277260 ns firing landed too, so
it is not a TCG signature and must not be read as one. And the sentence "a
scheduler pass that fits 200 µs natively" is false on both native measurements
there are.

## The panic is gone, and so is this file's headline

**Invariant P was deleted on 2026-08-17.** A pass is measured with wall clock,
and a guest's wall clock advances while a hypervisor holds its vCPU, so the
quantity carried a term the kernel neither observes nor controls; a check build
now records the distribution and the harness gates it at `MAX_PASS_NS` on the
90th percentile. The sibling file carries the design and the reasoning.

**The title of this file is now false, and the measurement that falsifies it also
explains it.** The dev host no longer dies — `sched_check_build` boots the check
kernel, runs `sched_stress` to completion and prints `all sched_stress tests
passed` — and **alone on a quiet host it passes the gate**. `cargo test` on
`wt/toyos-invariantp`, 2026-08-17, one suite, reports every 200 ms of guest time:

```
alone, host at 1.02x the reference boot (fastest boot 1350 ms against 1320 ms):
cpu0: 168 passes, p50 < 16384 ns, p90 < 131072 ns, p99 < 2097152 ns, max 1504209 ns, 7 over the 200000 ns budget
cpu1: 149 passes, p50 < 16384 ns, p90 < 131072 ns, p99 < 1048576 ns, max 1705237 ns, 7 over the 200000 ns budget

the same suite's 12-wide phase, same tree, minutes earlier:
cpu0: 134 passes, p50 < 131072 ns, p90 < 262144 ns, p99 < 2097152 ns, max 1745977 ns, 14 over the 200000 ns budget
cpu1: 140 passes, p50 < 131072 ns, p90 < 524288 ns, p99 < 2097152 ns, max 1983109 ns, 18 over the 200000 ns budget
```

**Host contention moves this guest's median by a factor of eight**, from under
16 µs to under 131 µs, and the 90th percentile with it. So cross-arch TCG on a
quiet machine *does* fit the budget for nine passes in ten; what did not fit was
TCG on a machine running eleven other guests. Everything above about the 1.68 ms
firings stands as a description of what the panic did — and stops being a claim
about what the emulator costs, because the panic reported one sample and this
reports the distribution.

Two consequences, both landed:

- **`sched_check_build` is `Sched::Serial`.** Its verdict is a wall-clock
  distribution, and `tests/toyos.rs`'s own rule is that such a test must have the
  machine to itself. The harness said so itself: *"it fails only beside other
  guests, so its `Sched::Parallel` is wrong"*.
- **The `Instrument::DevHostAlone` row in `src/redlist.rs` is retired** and a
  `DevHostLoaded` one takes its place. The dev host, alone and quiet, is green.

**Both rows are retired now, on 2026-08-18, and the second one by the experiment
this file's numbers argued for.** Quiet and loaded arms interleaved in one
session, twelve CPU-runs each: 0 of 12 quiet over the budget at the 90th
percentile against 9 of 12 loaded, 6 of 6 runs green against 6 of 6 red, with the
arms separated by boot width alone. The whole distribution translates one
power-of-two bucket under host load — so the harness stopped drawing a line over
this distribution on this accelerator at all. Cross-arch TCG now *reports* its
pass costs and judges no magnitude; `tests/common/passcost.rs` holds the
experiment and both recorded samples. The three things `sched_check_build` still
gates here are unchanged: a clean boot, the check-build asserts not firing, and
`sched_stress` running to completion.

With the reclassification in, the whole suite came back **263 of 263 green**, and
the serial tail's own report is the strongest single statement this file can
make:

```
cpu0: 147 passes, p50 < 32768 ns, p90 < 131072 ns, p99 < 2097152 ns, max 1974235 ns, 7 over the 200000 ns budget
cpu1: 160 passes, p50 < 16384 ns, p90 < 131072 ns, p99 < 1048576 ns, max 2543303 ns, 6 over the 200000 ns budget
```

**Thirteen passes over the budget, one of them 2 543 303 ns — twelve times it —
and the run is green.** Invariant P would have halted every CPU on the first of
those thirteen. That is the property the replacement was built for: a lone
enormous sample is the machine underneath, and mass is the scheduler.

An unexpected second finding, kept because the next reader will want it: the
per-CPU pass count over a whole boot plus `sched_stress` is about **150**, not
thousands. A quantum is 10 ms, so nearly every pass is a block or a wake rather
than a tick, and that number is what decides which quantile any gate over this
distribution can honestly state.

The related gap in the measured *window* — that the budget cannot see the xHCI
prologue at all — is `issues/kernel/scheduler-pass-blocks-in-xhci.md` and
is untouched by this. That issue named two halves wanting fixing together, "the
measured window has to start where the scheduler entry starts, and the gate has
to run somewhere". The second half is now done; the first is still open there.
