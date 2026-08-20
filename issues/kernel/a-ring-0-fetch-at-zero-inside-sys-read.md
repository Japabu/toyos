---
status: open
kind: defect
opened: 2026-08-19
---

# The fourth Ring 0 fetch at address 0, this time inside `SYS_READ`

One sighting, dev host, `wt/toyos-spawnrule`, 2026-08-19, in the shared
`tests/testcases` boot of a full `cargo test` — the run that reds is
`process_lifecycle`, twelve wide, with another worktree's suite holding all
twelve guest slots for part of it. The second full run of the same tree in the
same session was green, 268 of 268, and the first run's own re-run-alone of the
name was `PASS (491ms)`. So: **1 of 2 full runs, and the fourth sighting of the
class in `issues/kernel/the-shared-boot-jumped-to-null-spawning-sched-stress.md`.**

**This is the report that file's fix was written to produce.** It predicted that
a fourth sighting would print `KERNEL PANIC: execute unmapped address at 0x0`, a
kernel `rip`, a backtrace and the register dump, and be classified as
`Died::Kernel` rather than reported as a stall. All of that arrived:

```
[kernel 13.613 cpu0] spawn: /bin/test_rs_process_lifecycle pid=101 tid=0 base=0x10000000000 entry=0x10000013010 cr3=0x2cfc000 …
[kernel 13.660 cpu0] spawn: /bin/test_rs_process_lifecycle pid=102 tid=0 base=0x10000000000 entry=0x10000013010 cr3=0x8b8d000 …
[kernel 13.710 cpu1] #PF UNHANDLED: cr2=0x0 rip=0x0 err=0x10 user=false tid=Some(Tid(0))
[kernel 13.710 cpu1] FAULT rip=0x0 cr2=0x0 err=0x10 cr3=0x0000000002c4d002 rsp=0xffff800002c7d8a0 tid=0
[kernel 13.710 cpu1] KERNEL PANIC: execute unmapped address at 0x0
[kernel 13.710 cpu1]   Page walk for 0x0 [PML4=0x2c4d000 PCID=2 PML4[0] PDPT[0] PD[0] PT[0]]:
[kernel 13.710 cpu1]     PML4E: 0x0000000000000000 P=0 W=0 U=0
[kernel 13.710 cpu1]   Registers:
[kernel 13.710 cpu1]     rax=0x000000ffff5fff90  rbx=0x0000000000000000
[kernel 13.710 cpu1]     rcx=0x0000000000310668  rdx=0x000000008bbfd888
[kernel 13.710 cpu1]     rsi=0xffff800002c7d860  rdi=0xffff800000c00c10
[kernel 13.710 cpu1]     rbp=0x0000000000000000  rsp=0xffff800002c7d8a0
[kernel 13.710 cpu1]      r8=0xffff800000e41f88   r9=0x0000000000000000
[kernel 13.710 cpu1]     r10=0x0000000000000000  r11=0x0000000000000010
[kernel 13.710 cpu1]     r12=0xffff80007ce9c352  r13=0xffff800002c7d980
[kernel 13.710 cpu1]     r14=0xffff80007d12ad08  r15=0x0000000000000000
[kernel 13.710 cpu1]     cs=0x0008  ss=0x0010  rflags=0x0000000000010003
[kernel 13.710 cpu1]   Backtrace:
[kernel 13.710 cpu1]   Syscall: num=1 user_rip=0x10000098819 user_rsp=0xfffffffb60
[kernel 13.710 cpu1]   User backtrace:
[kernel 13.711 cpu1]     0x10000098819  toyos::net::tcp_accept+0xd9
[kernel 13.711 cpu1]   Stack (from RSP):
[kernel 13.711 cpu1]     [0xffff800002c7d8a0] = 0x0000000000010000
[kernel 13.711 cpu1]     [0xffff800002c7d8a8] = 0x0000000000030000
[kernel 13.711 cpu1]     [0xffff800002c7d8b0] = 0x0000000000017200
[kernel 13.711 cpu1]     [0xffff800002c7d8b8] = 0xffff800001631000
[kernel 13.711 cpu1]     [0xffff800002c7d8c0] = 0x0000000001631000
[kernel 13.711 cpu1]     [0xffff800002c7d8c8] = 0x000000000001000b
[kernel 13.711 cpu1]     [0xffff800002c7d8d0] = 0xffff800001610000
[kernel 13.711 cpu1]     [0xffff800002c7d8d8] = 0x0000000001610000
```

## What is new, against the three earlier sightings

**The call was in flight, not resumed.** `Syscall: num=1` is `SYS_READ`
(`toyos_abi::syscall::SYS_READ`), so the kernel was executing a read on behalf
of a userland thread when it fetched from zero. The two earlier zero-address
sightings carry no syscall at all.

**The reissued-stack mechanism does not fit this one.** The 2026-08-09 sighting
was diagnosed in its own file as `context_switch` resuming a task whose kernel
stack had been freed and handed out again — the evidence being an all-zero
restored frame: `rbp=0`, `rip=0`, `rflags=0x10002` (`popfq` of zero) and eight
zero quadwords from `RSP`. Here `rbp` and `rip` are zero and **the rest is not**:
`rflags=0x10003` has `CF` set, so the word that was popped was not zero, and the
eight quadwords below `RSP` hold plausible values — three of them kernel
addresses around `0xffff800001610000`, one repeated as its physical alias. A
zeroed page does not read like that. So either the mechanism is a different one
or the zeroing story is not what either sighting shows.

**`cr3` is a third address space.** The faulting `cr3` is `0x2c4d002` (PCID 2),
which is neither of the two processes spawned in the 120 ms before it
(`0x2cfc000`, `0x8b8d000`). Whatever was reading was already running.

**The user `rip` symbolises to `toyos::net::tcp_accept+0xd9`, and that is worth
nothing on its own** — it is the nearest preceding symbol in the test binary and
`process_lifecycle` does no networking. Recorded because the next sighting's
should be compared against it, not because it names anything.

## What it is not

The tree it was seen on carries one behaviour change: a spawn's slot map naming
a handle the parent does not hold now ends the parent instead of skipping the
pair (`kernel/src/loader/start.rs`). That path is `SYS_SPAWN`'s, it produces a
userland process kill and no kernel jump, no `handle fault:` line appears
anywhere in either run's output, and the fault above is inside `SYS_READ`. The
rest of the diff is comments and one test role.

## What is wanted

The same thing the third sighting asked for: **which null pointer, and whose.**
A Ring 0 fetch at zero is the kernel crashing from userland, which is the one
thing it may never do. The register state above is what a fifth sighting should
be compared against.
