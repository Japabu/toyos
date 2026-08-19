---
status: open
kind: defect
opened: 2026-08-19
---

# A second Ring 0 fetch at `0x1b`, and this one's `rsp` sits exactly on a page boundary

One sighting, dev host, `wt/toyos-spawnrule`, 2026-08-19, in a full `cargo test`
of the branch after it merged `origin/main` at `bf54143`. Host at 1.64x the
reference boot width, another worktree's suite on the machine. The name that
reds is `i8042_kbd_echo`; `ALONE i8042_kbd_echo: GREEN` in the same run.
It is the **second** sighting of `0x1b` —
`issues/kernel/a-ring-0-fetch-at-0x1b-during-a-loaded-boot.md` is the first,
2026-08-15, also during a loaded boot — and the first to arrive with the full
report the panic vocabulary now prints.

```
[kernel 0.850 cpu1] KERNEL PANIC: execute unmapped address at 0x1b
[kernel 0.850 cpu1]   Page walk for 0x1b [PML4=0x2690000 PCID=4 …]: PML4E: 0x0 P=0 W=0 U=0
[kernel 0.850 cpu1]     rax=0x000000ffff5fff90  rbx=0x0000000000000023
[kernel 0.850 cpu1]     rcx=0x0000000000310668  rdx=0x0000000004a5d76b
[kernel 0.850 cpu1]     rsi=0xffff8000026dffc0  rdi=0xffff800000c00a80
[kernel 0.850 cpu1]     rbp=0x0000000000010206  rsp=0xffff8000026e0000
[kernel 0.850 cpu1]      r8=0xffff800000e41f68   r9=0x0000000000000000
[kernel 0.850 cpu1]     r12=0x0000010000027e20  r13=0x0000000000000000
[kernel 0.850 cpu1]     r14=0x0000000000000008  r15=0x000000ffff000c50
[kernel 0.850 cpu1]     cs=0x0008  ss=0x0010  rflags=0x0000000000257a12
[kernel 0.850 cpu1]   Syscall: num=49 user_rip=0x1000005c632 user_rsp=0xfffffffc10
[kernel 0.854 cpu1] FAULT rip=0xffff80007ce97e6d cr2=0x41 err=0x0 cr3=0x2690004 rsp=0xffff8000026dfb00 tid=0
[kernel 0.854 cpu1] SEGFAULT tid=0: read unmapped address at 0x41
```

## What the registers say

**`rsp` is `0xffff8000026e0000` — exactly 4 KiB-aligned, and the eight quadwords
the report prints are all *above* it**, so the stack is empty at this point:
nothing was pushed under the frame that returned to `0x1b`.

**`rbp=0x10206` is not a pointer; it is an `RFLAGS` word** — `IF` set, bit 1
reserved-set, `PF` and `AF`. A restored register file where `rbp` holds RFLAGS
and `rip` holds a small integer is a `popfq`/`ret` sequence reading a frame that
is not the frame it laid down, which is the same family as the zero-address
sightings without being the all-zero form either of them showed.

**`cs=0x0008`, `user=false`**: Ring 0 executed at `0x1b`. `0x1b` is the user
code selector's value, so a plausible reading is a saved `cs` popped where a
`rip` belongs — one quadword of slip in a restored frame. Recorded as a reading
and not as a diagnosis.

**A second fault follows on the same CPU**: a kernel read of `0x41`, which is
the report machinery walking whatever it was handed. The user backtrace it
printed decodes to `gimli` symbols and then to bytes that are ASCII, so the
process's frame was not one either.

`num=49` is the syscall in flight; the guest is 850 ms into boot, so this is
`/bin/init`'s or a daemon's early work rather than any test's steady state —
`i8042_kbd_echo` is the name on the red because it was the workload, and the
kernel is what died.

## The session it was seen in

Five full `cargo test` runs of one branch on one dev host in one evening, on a
machine another worktree was also running a suite on:

| run | host width | outcome |
|---|---|---|
| 1 | 1.02x | `process_lifecycle`: Ring 0 fetch at `0x0` inside `SYS_READ` |
| 2 | 1.07x | **268 of 268 green** |
| 3 | 1.41x | `sched_stress`: `BTreeMap` iterator `unwrap` in a scheduler pass, 129 collateral reds |
| 4 | 1.64x | `i8042_kbd_echo`: this file |
| 5 | 1.23x | `screen_console_shell`: `typed \`echo zqjxk\` at the prompt and no row of the panel is its output` — 786 s against 2 s alone, the panel showing only the first frames of boot. A starved guest, and the only one of the five that is not a kernel death |

Every one of them re-ran `ALONE … GREEN`. The branch's whole behaviour change is
one line of `SYS_SPAWN`'s slot-map resolution, no `handle fault:` line appears in
any of the five runs, and the run that passed 268 of 268 compiled the same
statements as the two either side of it. **Three kernel deaths of three
different shapes in four contended suites is the finding**, and it is larger
than any of the three files that carry one of them:
this one, `issues/kernel/a-ring-0-fetch-at-zero-inside-sys-read.md` and
`issues/kernel/a-btreemap-panicked-inside-its-own-navigation-in-a-scheduler-pass.md`.
