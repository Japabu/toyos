---
status: open
kind: defect
opened: 2026-08-06
---

# A CPU that stops scheduling keeps publishing load 0, and `placement()` therefore prefers it

The T14's Ctrl+Alt+D dump (`boot9-dump.log`, 35.181 s) reports `5/8 cpu(s)
answered`. cpu1, cpu4 and cpu7 each failed to reach a scheduler pass inside the
dump's 250 ms budget. Their last lines in that whole 63 s boot are the idle
loop's own:

| CPU | last line, and it is the last thing it ever said |
|---|---|
| cpu1 | 1.152 s — `sched: cpu=1 ready=0 parked=0 current=None` |
| cpu7 | 11.231 s — `sched: cpu=7 ready=0 parked=1 current=None` |
| cpu4 | 26.110 s — `sched: cpu=4 ready=0 parked=1 current=None` |

**Whatever stops them is a separate defect. This entry is about what the
scheduler then does about it, which is the opposite of the right thing.**
`CpuHandle::publish_load` is written by a CPU at the end of each of its own
passes, so a CPU that takes no more passes publishes its last value forever —
and its last value is the one it wrote on the way into idle, which is **zero**.
`driver::placement` picks `min_by_key(load)`. A dead CPU is therefore not merely
a CPU that never runs anything again; it is the CPU the scheduler *prefers* for
every subsequent spawn.

That is the difference between losing a core and the machine getting
progressively worse, which is what the owner reports and what the log shows:
three cores shed over 26 s, and by the end the dump finds `pid=10 terminal`,
`pid=6 tid=2 doom` and `pid=12 shell` all `ready and has never run`, plus
`soundd` ready with 2 ms of CPU. doom's black window is its sound-init thread
placed on a shed core; the missing shell prompt is `shell pid=12` on another.

A load figure is a claim about the present, and a stopped CPU's is a lie the
scheduler believes. The fix is independent of the root cause and worth having
either way: placement must not be able to choose a CPU that has not completed a
pass in some number of intervals. The counter to compare against already exists
per CPU (`publish_load`'s call site is the end of every pass, so a monotonic
pass count published beside the load costs one relaxed store), and the same
staleness test would let `idle_sibling` and `post_steal_probe` stop aiming at
dead cores too.

Not fixed here: the instrument that names *why* a CPU stops (NMI probe,
`arch/idt/nmi.rs`) landed first, because a placement filter over an unknown
cause would hide the cause.
