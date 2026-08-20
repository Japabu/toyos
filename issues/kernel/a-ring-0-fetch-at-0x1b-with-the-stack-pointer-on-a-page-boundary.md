---
status: open
kind: defect
opened: 2026-08-19
---

# A shifted-frame fetch at `0x1b` on a syscall the other three do not share

One sighting, dev host, `wt/toyos-spawnrule`, 2026-08-19 22:53 UTC, in a full
`cargo test` of the branch after it merged `origin/main` at `bf54143`. Host at
1.64x the reference boot width, another worktree's suite on the machine. The
name that reds is `i8042_kbd_echo`; `ALONE i8042_kbd_echo: GREEN` in the same
run.

**The fourth instance of the shifted-frame half of the `0x1b` class.** The other
three are in `issues/kernel/a-ring-0-fetch-at-0x1b-during-a-loaded-boot.md`
(2026-08-15 `esp_filesystem`; 2026-08-19 22:03 `i8042_budget_expiry`, with one
guest on the machine and no load to appeal to; 2026-08-19 22:40
`nvme_large_device`). Filed apart rather than appended to that file because of
the last section — **this one falsifies its newest hypothesis**, and the two
sightings that hypothesis rests on landed on `main` in the same hour this one
was captured.

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

## What it agrees with, and the one thing it breaks

Against the three shifted-frame sightings in the class file, four things repeat
exactly and one does not:

| | 08-15 `esp_filesystem` | 08-19 22:03 `i8042_budget_expiry` | 08-19 22:40 `nvme_large_device` | this one |
|---|---|---|---|---|
| `rax` | `0xffff5fff90` | `0xffff5fff90` | `0xffff5fff90` | `0xffff5fff90` |
| `rbx` | `0x23` | `0x23` | `0x23` | `0x23` |
| `rbp` | `0x10246` | `0x10246` | `0x10297` | `0x10206` |
| `rflags` | `0x257842` | `0x257842` | `0x257bc2` | `0x257a12` |
| `rsp` | 4 KiB-aligned | same, and the same address | 4 KiB-aligned | 4 KiB-aligned |
| `[rsp+8]` | `0xc943` | `0xc983` | `0x16fb1` | `0xff1` |
| syscall | *(no line)* | `num=90` @ `0x1000003d598` | `num=90` @ `0x1000003d598` | **`num=49` @ `0x1000005c632`** |

`rax` and `rbx` are byte-identical across all four; `rbp` is an `RF`-set flags
word in every one; `rsp` is on a page boundary in every one; and one slot above
it sits a small non-address integer in every one.

**The syscall is where it parts.** That file's closing paragraph says *"every
shifted-frame sighting is a `spawn`-path syscall from the same instruction"* and
calls the shared `rip` the newest thread to pull. This sighting is `num=49`
(`SYS_NANOSLEEP`) from `user_rip=0x1000005c632`, symbolising inside `gimli`'s
DWARF reader — a different number, a different instruction, and not the spawn
path. So the shared `rip` is a property of two sightings and not of the
mechanism, and a fix aimed at it would be aimed at a coincidence. `num=90` is
`SYS_IO_URING_ENTER`, which `Command::spawn` reaches while it waits; a
`nanosleep` reaches the same scheduler entry by another door, which is the
weaker statement the four together actually support.

The guest is 850 ms into boot, so the faulting context is `/bin/init`'s or a
daemon's early work rather than any test's steady state — `i8042_kbd_echo` is
the name on the red because it was the workload, and the kernel is what died.

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

**And it is not one worktree's evening.** `wt/toyos-i8042deep` was on the same
machine for the same hours and took two `0x1b` deaths of its own — its 22:03 one
with a single guest on the host and no load to appeal to, which is the sighting
that took "under load" off the class's description. Five kernel deaths on one
dev host between 22:03 and 22:53 UTC, across two branches whose diffs share no
file.
