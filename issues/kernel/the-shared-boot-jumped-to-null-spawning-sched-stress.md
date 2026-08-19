---
status: open
kind: defect
opened: 2026-08-17
---

# The kernel fetched an instruction from address 0 while `sched_stress` spawned

One sighting, dev host, `wt/toyos-panicstall`, 2026-08-17, in the shared
`tests/testcases` boot of a full `cargo test`:

```
[kernel 11.438 cpu0] spawn: /bin/test_rs_sched_stress pid=114 …
[kernel 11.492 cpu1 tid=1] exit: test_rs_sched_stress tid=1 code=0 cpu=8ms
[kernel 11.684 cpu0 tid=2] exit: test_rs_sched_stress tid=2 code=0 cpu=5ms
[kernel 11.838 cpu0] spawn: /bin/test_rs_sched_stress pid=115 …
[kernel 11.847 cpu0] spawn: /bin/test_rs_sched_stress pid=116 …
[kernel 11.881 cpu0] spawn: /bin/test_rs_sched_stress pid=117 … (tls=15ms total=34ms)
[kernel 11.916 cpu1] #PF UNHANDLED: cr2=0x0 rip=0x0 err=0x10 user=false tid=Some(Tid(0))
```

and then nothing, for the whole 88 s of the test's guard. `err=0x10` is
instruction-fetch, not-present; `rip=0x0` with `user=false` is **Ring 0 having
jumped to address zero**. `sched_stress` was mid-spawn-storm — three processes
started in 43 ms and the last one's TLS setup alone took 15 ms of a 34 ms spawn,
so the machine was loaded.

**Something in the kernel called through a null function pointer, and nothing
here says what.** That is what is open. It is the third sighting of the class:
`issues/kernel/ring0-jump-to-zero-under-port-polls.md` (address 0, a
`sched_stress` thread, 2026-08-09) and
`issues/kernel/a-ring-0-fetch-at-0x1b-during-a-loaded-boot.md` (address
`0x1b`, a daemon's boot, 2026-08-15) are the other two, and the register state
in each is the evidence to compare a fourth against.

## Why this one carries no evidence at all, and why the next one will

The crash report never reached the console. `page_fault_handler` logged the line
above and fell into `fatal_exception`, whose first act is an `alert!("FAULT
rip=…")` and whose second is `crash_report`. Neither arrived, because
`is_user_fault()` answered *true*: it was `is_user_mode() || (PageFault &&
current_tid().is_some() && cr2 < 0x0000_8000_0000_0000)`, and with `cr2 = 0` and
a live tid the second disjunct held however Ring 0 the frame was. So the kernel
took the recovery path for a null jump, wedged, and said nothing more — a
machine that halts at least flushes.

**Fixed**, on the branch that carries this rewrite:
`toyos-userbound/src/fault.rs` is the classification now, `Ring` is
opaque and built from `cs` alone, and a Ring 0 frame whose `rip` is not a kernel
address is the kernel's whatever `cr2` holds. A fourth sighting therefore
prints `KERNEL PANIC: execute unmapped address at 0x0`, a kernel `rip`, a kernel
backtrace and the register dump, and then halts — which is also the line
`tests/common/serial.rs` already classifies as `Died::Kernel`, so the harness
names it by itself instead of reporting `STALLED`.

The two older sightings are the same misattribution seen from the other side:
both were reported as `SEGFAULT tid=N` with `cs=0x0008` in the register dump,
and both would now be reported as the kernel's.
