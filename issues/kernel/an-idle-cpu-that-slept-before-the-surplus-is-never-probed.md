---
status: open
kind: finding
opened: 2026-08-22
---

# An idle CPU that slept before the surplus appeared is never probed again

The balance path is pull-only. `SchedPass::post_steal_probe`
(`toyos-sched/src/cpu.rs`) runs on the idle path, picks the CPU publishing the
most surplus, and returns without posting anything if that surplus is under two.
The probe node is one-shot, so there is at most one outstanding per CPU, and
nothing re-posts it: `driver::execute`'s `Action::Idle` arm halts the CPU, and a
CPU with no run queue, no deadline and no device work stays halted until an
interrupt arrives. There is no push half — a CPU that gains surplus tells
nobody.

So a CPU that reached its idle pass while every other CPU still published a
surplus of zero, and then halted, is not given work by the balance path however
much surplus appears afterwards. What recovers it is a kick from somewhere else,
and every path that produces one is a path that was already giving that CPU the
work: spawn placement kicks the CPU it placed on, a wake kicks the waiter's home
CPU.

Measured, `toyos-sched/sim/tests/policy.rs`'s
`the_balance_path_drains_the_cpu_an_adversary_loaded` over 20 seeds per width,
with every thread spawned onto cpu0 (`workload::PlacementShape::AllOn`):

| cpus | threads | seeds reaching every CPU | seeds reaching a second CPU |
|---|---|---|---|
| 2 |  8 | 9/20 |  9/20 |
| 4 | 16 | 2/20 | 13/20 |
| 4 | 64 | 2/20 | 13/20 |
| 8 | 64 | 0/20 | 15/20 |

Where a thief was awake the drain is complete — the best seed at every width
moves exactly `threads − 2` tasks, cpu0's whole surplus above the floor
`answer_steal_requests` stops at. The first column is the CPUs that lost the
race, and it falls with width because all of them have to win it.

**Reachable in the kernel, not only in the model.** A wake posts to the waiter's
*home* CPU whatever that CPU's load is, and a home is where spawn placement put
the thread when it was created. A process whose threads were all placed on one
CPU while the machine was otherwise busy, parked, and then woken together, is
exactly the state above: one CPU with N runnable threads and the sleepers with
no probe outstanding. Nothing has measured how often that composition occurs on
a real boot.

Not filed as a defect because no shipped workload has been shown to reach it,
and the fix is a design decision the balance path does not currently make: a
push on surplus, or a bounded re-arm of the probe, both of which cost an idle
CPU wakes it is currently right to be without
(`kernel/CLAUDE.md`: anything added to the idle loop is an audio change).
