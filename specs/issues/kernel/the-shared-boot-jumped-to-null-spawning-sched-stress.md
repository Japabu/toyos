---
status: open
kind: defect
opened: 2026-08-17
---

# The kernel fetched an instruction from address 0 while `sched_stress` spawned, and the crash report never arrived

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

**Two things are wrong and they are separable.**

1. **Something in the kernel called through a null function pointer.** That is
   the defect; nothing here says what.

2. **The crash report never reached the console.** `page_fault_handler` logs the
   line above and then falls into `fatal_exception`, whose very first act is an
   `alert!("FAULT rip=…")` and whose second is `crash_report`. Neither arrived.
   One reading is that `is_user_fault()` answered *true* — it is
   `is_user_mode() || (PageFault && current_tid().is_some() && cr2 <
   0x0000_8000_0000_0000)`, and with `cr2 = 0` and a live tid the second
   disjunct holds however Ring 0 the frame was — so the kernel took
   `recover_or_halt(is_user = true, is_ring3 = false)`, tried to recover a
   *kernel* null jump through `try_recover_from_panic`, and wedged instead of
   halting. A machine that halts at least flushes; this one said nothing more.

Item 2 is what makes item 1 unbisectable, and it is the more general defect: a
Ring 0 instruction fetch from a low address is not a user fault whatever
`current_tid()` says, and `cr2 < user_max` cannot be the test that decides it.

## What this cost

The stall took the shared boot with it and the harness's reboot then raced the
guest it was replacing, so 129 further tests were reported red on the same
sentence:
`specs/issues/build/the-shared-boot-reboot-races-the-guest-it-replaces.md`.

## What the harness now says about it, and what it still cannot

It still reports `STALLED`, and correctly: `#PF UNHANDLED` is printed for a user
fault and a kernel one alike — `tests/common/faults.rs` stages the user case on
purpose — so the spelling cannot be added to `tests/common/serial.rs`'s table
without misclassifying it. The line that *would* have been unambiguous is the
`KERNEL PANIC` this boot never got to write, which is item 2. Fix item 2 and the
harness names this by itself.
