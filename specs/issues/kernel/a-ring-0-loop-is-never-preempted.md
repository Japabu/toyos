---
status: open
kind: finding
opened: 2026-08-15
---

# A kernel thread in a Ring 0 loop is never preempted, and never migrates

`sched::kthread`'s module header says a kernel thread "is preemptible, it is
stealable". The first half is not true of a thread that does not return to Ring
3 and takes no lock, and the second follows from it: a **running** task is never
stolen, only a ready one, so a thread that is never made ready on somebody
else's behalf never moves.

**Why the first half fails.** `need_resched` is set by the timer's Ring 0 stub
(`arch/idt/timer.rs`'s `2:` branch) and consumed in exactly two places —
`kernel_exit_to_user_check`, on the way back to Ring 3, and `preempt::enable`'s
slow path when the count drops to zero. A kernel thread's body reaches neither:
it never leaves Ring 0, and a loop that takes no `Lock` never calls
`preempt::enable` at all. The stub sets the byte on every tick and nothing ever
reads it.

**Measured, on `wt/toyos-logd4` at `a0729cf`, 2026-08-15.** The `log-storm`
actuator (`kernel/src/log/storm.rs`) spawns one kernel thread per CPU, each
emitting patterned log records; the in-guest gate counts producers whose records
were found on more than one shard, which is exactly "this producer ran on more
than one CPU". Three shapes at `--smp 8`:

| shape | producers on a second shard |
|---|---|
| one thread per shard, a tight loop | 0 of 8 |
| two per shard, `yield_now` every 16 records | 0 of 16 |
| two per shard, a 50 µs park every 16 records | 0 of 16 |

The third is the interesting one: a park *does* reach the scheduler, and the
task still came back to the CPU it left — so on this tree a woken kernel thread
is placed where it was, and the load imbalance that would make a sibling steal
it never arises while every CPU has work.

**What it cost.** A planned `log_migration_storm` gate
was written as "`log-storm` from kernel threads … at `--smp 8` with stealing on,
so a producer is preempted and re-runs on a sibling". No workload of that shape
exists on this tree, so the gate was not built; the nesting gate — an
interrupt that logs, on one CPU — is what reaches the reservation race instead,
and both of §9.4's reservation controls are demonstrated through it.

**A second, smaller fact found with it.** `scheduler::yield_now` asserts
`BASELINE_TRAP`, which is the depth a *syscall* reaches it at; a kernel thread's
body runs at zero and trips the check on a caller holding no lock at all.
`blocking_baseline()` already carries the argument for reading the entitlement
off the context instead — `yield_now` is the one scheduling entry that does not
use it. Nothing calls it from a kernel thread today, which is why this is a
finding and not a defect.
