---
status: closed
kind: defect
opened: 2026-08-17
closed: 2026-08-17
---

> **Closed by the change this file asked for.** Invariant P no longer exists:
> `toyos-sched`'s check build records the pass-cost distribution
> (`cpu::PassCosts`) and publishes it, and `tests/common/passcost.rs` gates it.
> The "what is owed" section below is item 1 delivered; item 2 is still filed
> with its own owner. What the gate claims, and why it claims a fraction rather
> than every pass, is at the end.

# The check-build guest stopped answering on KVM because invariant P fired, twice in 91 runs

`sched_check_build` STALLed on a KVM shard and came back `ALONE: GREEN`. The
question this file was opened to hold was whether that is a real hang or a guard
too tight for the thing it guards.

**It is neither: the guest panicked.** Invariant P fired, the panic path halted
every CPU, and the silence the guard measured is a dead machine that had already
said why it died.

**Three sightings, two of them decided.**

| run | when (UTC) | branch | firing |
|---|---|---|---|
| 31890991692 | 08-15 14:52 | `wt/toyos-logd56` | **unrecorded** — empty `serial:` |
| 31936533470 | 08-16 08:28 | **`main`**, push | `277260 ns`, cpu1, `driver::idle_loop` |
| 31946183485 | 08-16 12:07 | `wt/toyos-harness2` (#95) | `200569 ns`, cpu0, `timer_handler` |

**Rate: 2 of 91.** A survey of the 100 most recent `ci` runs (2026-08-15 16:57 →
2026-08-17 13:29) found 91 in which the shard carrying this name actually ran it;
two fired invariant P, and both left the panic in the capture. Nine were excluded
as non-samples, their shard having been cancelled before the suite ran. The 08-15
sighting predates that window and is in neither number.

It reds `main` as readily as a branch: the 08-16 08:28 firing is a push to `main`.

The 08-16 12:07 capture, printed by the harness inside the message that called it
a stall:

```
[kernel 1.449 cpu0] PANIC: panicked at /__w/toyos/toyos/toyos-sched/src/cpu.rs:1094:5:
invariant P: a scheduler pass took 200569 ns, budget 200000 ns — the simulator
charges a pass nothing, so invariant I4's RT wake-latency bound is optimistic by
at least that much
[kernel 1.449 cpu0]   Backtrace:
[kernel 1.449 cpu0]     <toyos_sched::cpu::SchedPass<kernel::hw::KernelHw, kernel::sched::driver::PreemptOff, toyos_sched::cpu::Disposed>>::finish+0xa33
[kernel 1.449 cpu0]     kernel::sched::driver::with_cpu::<toyos_sched::cpu::Action<..>, kernel::sched::driver::pass::{closure#0}>+0x180
[kernel 1.449 cpu0]     kernel::sched::driver::pass+0x94
[kernel 1.449 cpu0]     kernel::arch::idt::timer::timer_handler+0x90
[kernel 1.449 cpu0]     kernel::arch::idt::timer::timer_entry+0x58
[kernel 1.449 cpu0]   Running: pid=7 tid=Some(Tid(0))
[kernel 1.449 cpu0]   Process: test_rs_sched_stress pid=7 state=Live
[kernel 1.449 cpu0]   Syscall: num=8 user_rip=0x100000387dd user_rsp=0xffffffd0f0
[kernel 1.449 cpu0]   User backtrace:
[kernel 1.449 cpu0]     <std::time::Instant>::elapsed+0x1d
[kernel 1.449 cpu0]     sched_stress::main+0x16e2
[kernel 1.450 cpu0] schedule_no_return: panicked inside a pass, cannot rejoin
```

Run 31946183485 (pull request #95, `wt/toyos-harness2`, sha `4ec5d01`), job
95162423932, `guest (8)`, 2026-08-16. The runner is an Azure Hosted Compute
Agent in `northcentralus`, `cpu: AMD EPYC 7763 64-Core Processor, 4 core(s)`,
`/dev/kvm` passed into the job container — so the ToyOS guest runs under KVM
inside a hypervisor guest, not on metal.

**The guest was not alive for those 383 s and it was not hung.** `PANIC` inside
a pass reaches `kernel/src/scheduler.rs`'s `schedule_no_return`, whose
`in_schedule_self()` arm calls `halt_all_cpus()`. Every CPU stopped 1 ms after
the assert fired, at 1.450 s of guest uptime — roughly 0.3 s into a 382 s guard.
The silence the guard measured is a machine that had already said why it
stopped.

The other firing, run 31936533470, job 95139261820, `guest (8)`, a push to
**`main`** — a different call site, a different CPU and a different runner SKU:

```
[kernel 1.140 cpu1] PANIC: panicked at /__w/toyos/toyos/toyos-sched/src/cpu.rs:1094:5:
invariant P: a scheduler pass took 277260 ns, budget 200000 ns
[kernel 1.140 cpu1]   Backtrace:
[kernel 1.140 cpu1]     <toyos_sched::cpu::SchedPass<..>>::finish+0xa33
[kernel 1.140 cpu1]     kernel::sched::driver::with_cpu::<..>+0x180
[kernel 1.140 cpu1]     kernel::sched::driver::pass+0x94
[kernel 1.140 cpu1]     kernel::sched::driver::idle_loop+0x26
```

`cpu: AMD EPYC 9V74 80-Core Processor, 4 core(s)`, against the 7763 above.
`STALL sched_check_build (392s)` of a 387 s guard, then `PASS (6s)` and
`ALONE: GREEN`.

**This is the case `invariant-p-cannot-hold-under-cross-arch-tcg.md` wrote down
in advance.** That file scopes the dev host's 1.68 ms firing to
`Instrument::DevHostAlone` and then says: "if invariant P ever fires on a **KVM**
shard, this file does not cover it and it is a real finding about the
scheduler." It has, twice.

**The call site does not separate the accelerators, and the magnitude does.** The
KVM firings are at *both* sites — `idle_loop`, which is where TCG fires, and
`timer_handler` during `sched_stress`. What differs is scale: 200569 and 277260
ns on KVM, against 1684167 and 1749243 ns on TCG. Two regimes, 1.0–1.4× budget
and 8.4–8.7×, with nothing observed in between.

## Why the harness called a panic a stall

*As it was on 2026-08-16. It is not that any more — see item 2 of what is owed —
and the paragraphs below are kept as the record of what produced this verdict.*

`tests/common/qemu.rs` holds two waits with two different vocabularies for a
fatal line, and only one of them is complete.

- `wait_for_ready` — the boot half — ends on `SEGFAULT`, `KERNEL PANIC` **or**
  `PANIC:`, drains two more seconds to collect the backtrace, and reports
  `Init process crashed during boot`. It is armed here: that arm is gated on
  `panic_aborts`, which is `ready == DEFAULT_READY`, and this test boots with a
  default `BootOptions`.
- `run_test_paced` — the test half — ends early on `KERNEL PANIC` and nothing
  else.

`KERNEL PANIC` is printed by `kernel/src/arch/idt/exceptions.rs:196-197` and
only there: it is the CPU-exception path. A Rust `panic!` — which is what every
`feature = "check"` scheduler assert is — goes to `crash_report_panic`, which
prints `alert!("PANIC: {}", info)` at line 272 of the same file. So
`run_test_paced`'s early-out cannot match a scheduler assert, and the wait ran
to its full ceiling on a machine that had been halted since second two.

Everything downstream then read the elapsed clock instead of the capture. The
red was classified `STALLED:`, the shard summary said *"1 of those reds are
blown liveness guards, not answers … the run established nothing about this tree
and there is nothing in it to bisect"*, and `durations` reported
`sched_check_build measured 387502 ms in CI, over the 10000 ms line` — the
guard's own arithmetic, filed as if it were a slow test. The run had in fact
established a great deal, and the thing to bisect was quoted four lines below the
sentence saying there was none.

## What is decided, and what is not

**Decided.** The 200 µs budget is not met on CI's KVM shards. `MAX_PASS_NS` is
a policy number whose own doc comment says "if it ever fires on honest work, the
honest response is to find out which pass grew and why — not to raise it", and
this was honest work: `sched_stress` spawning its burners.

**Not decided: whether the pass spent that time.** `check_pass_duration` reads
`hw.now()`, which is `kernel/src/clock.rs`'s `nanos_since_boot` — `rdtsc`
scaled by a calibrated period. Under KVM the guest TSC is the host's, offset by
a constant, and it keeps counting while the vCPU is not running. The pass holds
`PreemptOff` inside the guest, which stops the guest's scheduler and stops
nothing above it. So `elapsed` is wall clock across the pass, and on a runner
that is itself a hypervisor guest it includes any interval the host took the
vCPU away. The kernel cannot tell a 280 µs pass from a 5 µs pass plus 275 µs of
steal.

Two observations lean, weakly, toward the cost being real, and neither is strong
enough to close it:

- **The two firings cluster.** 200569 and 277260 ns, on two different EPYC SKUs.
  A host preemption is a millisecond-scale event; two samples inside 1.4× of the
  line look more like a distribution whose tail crosses it. **But the sample is
  truncated at 200000 ns by construction** — the assert only speaks when it
  exceeds — so the cluster's lower edge is an artefact of the instrument and only
  the absence of a millisecond-scale KVM firing in 91 runs is informative.
- **Both landed in the same phase**, 1.140 s and 1.449 s of guest uptime, inside
  `sched_stress`'s spawn burst rather than spread over the ~6 s the test runs.
  Two samples cannot distinguish that from chance.

The dev host cannot settle it. It is cross-arch TCG on arm64 and boots no KVM
guest at all, so a green local run of `sched_check_build` is not evidence about
this and must not be reported as any.

**The instrument that would decide it.** The check build should keep the
*distribution* of pass costs, not just the overshoot — a per-CPU maximum and a
coarse histogram, published at the end of a run. If the mass sits at single-digit
microseconds and the only sample above 100 µs is the one that panicked, the time
came from outside the guest and invariant P is asserting something a hosted CPU
cannot promise. If there is mass in the 100–200 µs band, the pass really does
cost that on this hardware and the question becomes which part of it grew. The
assert already sits at exactly the right place to take the sample; today it
throws every measurement away except the one that kills the machine.

The cost is measured on both: the 08-16 08:28 shard spent 544.9 s on its parallel
phase, the 12:07 one 481.2 s, in each case for six tests whose other five
measured 3–5 s.

## The 08-15 sighting is still undecided, and the harness is why

Run 31890991692, job 95027203184, `guest (8)` (`wt/toyos-logd56`):
`STALLED: 259s of guard expired, and the guest had said nothing for the last
259s of it`, `STALL sched_check_build (265s)`, then `PASS (6s)` and `ALONE:
GREEN`. Its `serial:` block is **empty**, so `run_test_paced` never saw
`===TEST_START test_rs_sched_stress===` and `in_test` stayed false — which means
every console line that boot produced, panic included if there was one, went to
`TestResult::before`, and the `sched_check_build` arm prints only
`result.serial`. The evidence existed and the caller dropped it.

The uploaded UART artifact does not fill the gap either: `shard-8-serial` carries
the 16550 log, which ends where the kernel switches to virtio-console at 0.377 s
of guest uptime, long before any of this. Both boots of that job reach the switch
normally.

**The shard-8 coincidence carries no information, but not because the shard is
fixed.** A shard is not a draw: `src/testargs.rs`'s `Shard::keep` is
longest-processing-time bin-packing over the checked-in duration profile, run
identically by every runner. It is not a constant either — the 91-run survey
found this name on **shard 7, 8 and 10** across 44 hours, moving when the profile
moved (shard 10 once, on `wt/toyos-suitecut`, a branch that edits fast-tier
membership). What it *is* is stable for a given profile: it sat on shard 8 for
79 consecutive samples, a band that contains all three sightings. So all three
landing on 8 is where the packing put the name on those days, not one chance in
twelve three times — and the shard index is not a variable worth carrying.

## What is owed

1. **`toyos-sched`** — the pass-cost distribution above, in `feature = "check"`.
   It is the only thing that turns this from a rate into a cause. **Done**, and
   what it turned into is below.
2. **`tests/common/qemu.rs`** — half done. `run_test_paced`, `wait_for_ready`
   and `await_guest` now read one vocabulary in `tests/common/serial.rs`, so a
   kernel panic during a test ends the run at the silence bound and the verdict
   names the panic instead of the guard; a *staged* one was measured at 18 s
   against 171 s of ceiling. The half still owed is the other one —
   every caller that reports `result.error` should report `before` and `started`
   with it, which is what would have made the first sighting readable:
   `issues/build/a-failure-message-drops-the-lines-before-the-test-started.md`.
   Nothing in this file waits on either; the verdict above is already decided
   without them.

## What the measurement became, and what is claimed now

Invariant P is deleted. `CpuHandle` carries a `PassCosts` in a
`feature = "check"` build — a per-CPU count, maximum, exact over-budget count
and a power-of-two histogram, written by the owning CPU with relaxed
load/stores. `SchedPass::finish` records into it where it used to assert.
`kernel/src/sched/driver.rs` publishes one line per CPU at most every 200 ms of
guest time, and `tests/common/passcost.rs` reads and judges it.

**The judgement was one claim: nine passes in ten are provably under
`MAX_PASS_NS`.** Every term of it was derived rather than picked — and the
magnitude has since been replaced, for the reason two paragraphs down:

- The magnitude was `MAX_PASS_NS` unchanged. The bound the panic stood over was
  the bound the gate stood over.
- The fraction is not *all* passes, because "not decided" above cannot be
  decided: there is no magnitude a hypervisor cannot produce by taking a vCPU
  away, so no bound over the maximum is a statement about the scheduler. That is
  precisely what removing the panic gives up, and it is the whole of it.
- The fraction is not ninety-nine in a hundred either, and the reason is sample
  size rather than comfort. A whole boot plus `sched_stress` takes about 150
  passes per CPU — measured on the dev host, 2026-08-17: 168, 149, 152, 152,
  142, 139, 134 and 129 across two sessions. A quantum is 10 ms, so nearly every
  pass is a block or a wake and not a tick. A 90th percentile over 150 samples
  has fifteen above it; a 99th has one and a half.

**The fraction was where this file's reasoning was weakest, and it has since
been measured — with the unfavourable answer.** "Nine in ten" rested on the claim
that a busy host reaches a handful of passes rather than a tenth of them: an
*observed rate* on this infrastructure, not a bound, and one the two red runs
above could not support, because the machine halted at the first crossing and
could never have shown a second — the counts censored where the magnitudes were.
The controlled experiment ran on 2026-08-18, quiet and loaded arms interleaved in
one session, twelve CPU-runs each, and **host load moves every order statistic,
the median as much as the tail**: 0 of 12 quiet CPU-runs over the budget at the
90th percentile against 9 of 12 loaded, 6 of 6 runs green against 6 of 6 red. No
fraction chosen instead of nine-in-ten would have survived.

So the *magnitude* moved instead of the fraction. A run is judged against what
its own accelerator has been recorded producing, and where a recorded sample
supports no line at all — cross-arch TCG, whose sample spans four buckets on one
unchanged tree — the distribution is reported and no verdict is taken.
`tests/common/passcost.rs` carries the experiment, both recorded samples and the
reasoning. Reading KVM's paravirtual steal-time MSR, the other instrument that
would have settled it, is closed by owner ruling: a hypervisor-specific facility
cannot be the basis of a gate in a tree whose north star is metal.

**Why the gate is green where the assert was, on the same evidence that opened
this file.** Invariant P asserted every pass under 200 000 ns and was green on 89
of these 91 KVM runs. In each of those 89, every sample was under the budget, so
the 90th percentile was. In the two that fired, one crossing out of ~150 samples
per CPU cannot move a 90th percentile. **The gate therefore passes on all 91,
including the two the assert halted the machine on** — which is the direction
that mattered and the direction no instrument available here could stage
directly.

**The cadence was measured too, and the first attempt at it was wrong.** A
report every *N passes* is a feedback loop: a report is a log record, a record
wakes `klogd`, and a wake is a pass. At one report per 64 passes cpu0 finished
the same workload at 1,408 passes; at one per 256 it never reached 256 at all.
The shipped cadence is wall clock, which nothing the reports do moves.

**And the dev host demonstrates the direction that mattered.** In the serial
tail of a run that ended 263 of 263 green:

```
cpu0: 147 passes, p50 < 32768 ns, p90 < 131072 ns, p99 < 2097152 ns, max 1974235 ns, 7 over the 200000 ns budget
cpu1: 160 passes, p50 < 16384 ns, p90 < 131072 ns, p99 < 1048576 ns, max 2543303 ns, 6 over the 200000 ns budget
```

**Thirteen passes over the budget, one of them twelve times it, and the test is
green.** Invariant P would have halted every CPU on the first of the thirteen —
which is the failure this file opened on, staged here by an emulator instead of a
hypervisor and answered instead of reproduced.

The other direction, on the same tree minutes earlier in the same suite's 12-wide
phase: `cpu0: 134 passes, p50 < 131072 ns, p90 < 262144 ns, max 1745977 ns, 14
over`, and the gate reds. The maxima on the two sides are 1.7 ms and 2.5 ms, and
the verdicts are opposite: **a lone enormous sample is the machine underneath,
and mass is the scheduler.** The consequence landed with it — the test is
`Sched::Serial` now, because a verdict that is a wall-clock distribution must
have the machine to itself.

**What the gate does not catch, stated rather than left to be discovered.** A
single pass that really ran long and a single pass whose CPU was descheduled are
the same sample, and nothing separates them — so a rare long pass is *reported*
(the maximum and the exact over-budget count are in every line the test prints)
and not gated. A pass that grew but stayed under the budget is reported the same
way, in the median. Both are visible every run, which is more than the assert
ever left behind: it threw every measurement away except the one that killed the
machine.
