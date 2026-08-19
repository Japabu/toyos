---
status: open
kind: defect
opened: 2026-08-15
---

# `tlb_shootdown_waits`' `munmap` stage can be vacuous, and it reds when it is

`tests/toyos-rust-tests/src/bin/tlb_shootdown_waits.rs` arms `SYS_DEBUG` action
12, which makes the last CPU an initiator waits for acknowledge a shootdown
`DELAY_NANOS` (20 ms) late, and then asserts that four operations take at least
`FLOOR_NANOS` (10 ms):

1. the bare shootdown, timed **inside the kernel** by the arming call itself;
2. `munmap` of a 2 MiB anonymous region, timed from userland;
3. a `MAP_FIXED` remap over a live range, timed from userland;
4. the same `munmap` with the delay disarmed, asserted *under* the floor.

Stages 1, 3 and 4 are sound. **Stage 2 has a precondition nothing in the test
establishes: that another CPU is holding this address space when the shootdown
goes out.** A shootdown IPI goes to the CPUs that have the mapping loaded; if
this process is resident on exactly one CPU at that instant, there is nobody to
wait for, the initiator waits for nothing, and `munmap` returns in microseconds —
which the assertion reads as "it freed the pages without waiting for the flush".

The comment at the site says *"a sibling thread of this process holds
translations for exactly this range"*. The binary has one thread. What actually
puts a second CPU into the shootdown set is incidental — a scheduler placement,
another process sharing nothing, or the CPU the test was last on still carrying
the address space — and none of it is arranged, asserted or reported.

## The observation

**CI run 31890991692**: `munmap returned in 33000ns with the last CPU answering
20000000ns late — it freed the pages without waiting for the flush`. 33 µs is
not a short wait; it is no wait, which is exactly what a shootdown with an empty
target set costs.

**Dev host, 17 samples in one session, all green** — and that is not evidence
against the above, because a dev-host guest under TCG runs the whole thing on a
machine whose scheduler placement happens to keep a sibling resident. It is one
CI observation with no denominator: nothing has been run repeatedly enough on
KVM to give the stage a rate, which is why there is **no `src/redlist.rs` row**
for it. A row without a denominator is a claim this tree does not make.

## What would fix it

The stage needs the precondition to be a property rather than a hope. The
cheapest honest shape is for the binary to spawn a sibling *thread* that touches
the region and then parks on another CPU — and to assert that it is on another
CPU, since nothing in this kernel pins a task. The stronger shape is for the
kernel to report the size of the shootdown target set through the same
`SYS_DEBUG` action, so the test can refuse to judge a shootdown that had nobody
to wait for instead of reading it as a failure. Either one turns "33 µs" from a
red into a skip with a reason.

## Where this came from

Found adjudicating the landing gate of PR #82 (the log architecture branch).
**Nothing on that branch touches the memory boundary, `SYS_DEBUG`, the
scheduler's placement or `munmap`**, and the red is not that branch's. Filed
rather than fixed for the same reason: the reasoning this stage implements
belongs to the memory-safety track.
