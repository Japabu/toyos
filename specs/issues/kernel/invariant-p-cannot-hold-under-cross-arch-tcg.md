---
status: open
kind: finding
opened: 2026-08-15
---

# Invariant P's 200 µs budget cannot be met on the dev host, so `sched_check_build` is a CI-only gate

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
`specs/issues/kernel/the-check-build-guest-stopped-answering-on-kvm-twice.md`
carries it.

**Magnitude does not disturb this file; two other things do.** 1.68 ms is five
times the largest KVM firing, and nothing on KVM has come near it, so the TCG
explanation of *that* gap stands. But the **call site** no longer separates the
accelerators — `driver::idle_loop` is where the 277260 ns firing landed too, so
it is not a TCG signature and must not be read as one. And the sentence "a
scheduler pass that fits 200 µs natively" is false on both native measurements
there are.

The related gap in the measured *window* — that the budget cannot see the xHCI
prologue at all — is `specs/issues/kernel/scheduler-pass-blocks-in-xhci.md` and
is untouched by this. That issue named two halves wanting fixing together, "the
measured window has to start where the scheduler entry starts, and the gate has
to run somewhere". The second half is now done; the first is still open there.
