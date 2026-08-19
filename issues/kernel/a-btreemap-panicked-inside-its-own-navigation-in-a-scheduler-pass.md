---
status: open
kind: defect
opened: 2026-08-19
---

# A CPU's `parked` map panicked inside `BTreeMap`'s own iterator, and took the boot

One sighting, dev host, `wt/toyos-spawnrule`, 2026-08-19, the third of three full
`cargo test` runs in one session — the most loaded of the three, `fastest boot
1867 ms against the reference 1320 ms`, 1.41x width. The name that reds is
`sched_stress`; `ALONE sched_stress: GREEN` and `PASS (2s)` in the same run.
**The shared `tests/testcases` boot died with it and took 129 other names down as
`Failed to flush QEMU stdin: … BrokenPipe`, so 130 of the run's 268 reds are one
event.**

```
[kernel 14.573 cpu1 tid=10] exit: test_rs_sched_stress tid=10 code=0 cpu=22ms
[kernel 14.589 cpu0] PANIC: panicked at library/alloc/src/collections/btree/navigate.rs:161:36:
called `Option::unwrap()` on a `None` value
[kernel 14.589 cpu0]   Backtrace:
[kernel 14.589 cpu0]     0xffff80007cf49ccc  core::panicking::panic_fmt+0x2c
[kernel 14.589 cpu0]     0xffff80007cf49c92  core::panicking::panic+0x12
[kernel 14.589 cpu0]     0xffff80007cf49a29  core::option::unwrap_failed+0x19
[kernel 14.590 cpu0]     0xffff80007cede152  <toyos_sched::cpu::SchedPass<kernel::hw::KernelHw, kernel::sched::driver::PreemptOff, toyos_sched::cpu::Disposed>>::apply_timer+0x2e2
[kernel 14.590 cpu0]   Running: pid=2 tid=Some(Tid(0))
[kernel 14.590 cpu0]   Syscall: num=15 user_rip=0x1000009cb88 user_rsp=0xfffffff860
[kernel 14.591 cpu0]     0x1000009cb88  <symbol unread: the process is gone from the table>
[kernel 14.592 cpu0] schedule_no_return: panicked inside a pass, cannot rejoin
```

## What the line says, which is more than "a panic in the scheduler"

`navigate.rs:161:36` is `LazyLeafRange::<Immut>::next_unchecked`'s
`self.init_front().unwrap()` — **inside `BTreeMap`'s own iterator**, not in any
code this tree wrote. That `unwrap` is unreachable for a well-formed map: it is
reached when the map's `length` says another element is owed and its node
structure has none left. `apply_timer` reads exactly one map, immutably —
`SchedCpu::earliest_deadline` is `self.parked.values().filter_map(…).min()`, and
`parked` is `BTreeMap<TaskKey, ParkedEntry<X>>` (`toyos-sched/src/cpu.rs`).

So this is not a logic bug in `apply_timer`, an absent deadline or an empty map:
**a CPU's park map disagreed with itself while a pass walked it.** Either the
map was mutated through an alias while that iterator was live, or the memory
under it was written by something else. `sched_stress` had just retired seven
threads and four processes in the 90 ms before it, which is the churn that fills
and empties `parked`.

`num=15` is `SYS_FSYNC` and pid 2 is a daemon, not `sched_stress` — the pass
that died was reached from an unrelated process's syscall, as any pass can be.
The workload is the name on the red; the map belongs to the CPU.

## Why this is not the branch it was seen on

The branch's whole behaviour change is one line of `SYS_SPAWN`'s slot-map
resolution (`kernel/src/loader/start.rs`). The second run of the same session
was **268 of 268 green**, and the kernel source between that run and this one
differs by a module doc comment and nothing else — no statement compiled
differently. No `handle fault:` line appears anywhere in the run.

## The family

The same session's first run died differently and is
`issues/kernel/a-ring-0-fetch-at-zero-inside-sys-read.md`; the zero-address
sightings before it are
`issues/kernel/the-shared-boot-jumped-to-null-spawning-sched-stress.md`,
`issues/kernel/ring0-jump-to-zero-under-port-polls.md` and
`issues/kernel/a-ring-0-fetch-at-0x1b-during-a-loaded-boot.md`. Two kernel deaths
in three runs of one tree, both under host contention, both `ALONE … GREEN`, and
neither of them a verdict any test wrote. **Whether they are one defect is not
decided here** — this one has a corrupted container and a name for it, which is
the first of the family to carry that much.
