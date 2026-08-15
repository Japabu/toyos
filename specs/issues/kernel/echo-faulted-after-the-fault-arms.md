---
status: open
kind: finding
opened: 2026-08-15
---

# A process spawned after `fault_gates`' arms read address 1, and the guest went with it

One run, on a host carrying two other agents' suites (`fastest boot 2119 ms
against the reference 1320 ms` on a filtered re-run minutes later). Seen once,
not reproduced by the full suite immediately after it, which was 253/253 green.

`fault_gates` raises every Ring 3 fault it has an arm for and then spawns
`/bin/echo` as its liveness probe — "the machine survived every fault but can no
longer start a process" is the assertion. On this run the probe itself faulted:

```
[kernel 4.828 cpu1] spawn: /bin/echo pid=69 tid=0 base=0x10000000000 entry=0x1000002ebd4 cr3=0x2ce6000 ...
[kernel 4.951 cpu1] #PF UNHANDLED: cr2=0x1 rip=0x100000328dd err=0x4 user=true tid=Some(Tid(0))
[kernel 4.951 cpu1] SEGFAULT tid=0: read unmapped address at 0x1
[kernel 4.951 cpu1]   Page walk for 0x1 [PML4=0x2cd1000 PCID=66 PML4[0] PDPT[0] PD[0] PT[0]]:
[kernel 4.951 cpu1]     PML4E: 0x0000000000000000 P=0 W=0 U=0
[kernel 4.951 cpu1]   Page fault trace (1 total, last 1):
[kernel 4.951 cpu1]     fault=0x1000000739c elf_off=0x0 blk=524288 relocs=2600 15495us [W   ]
```

`rax=0x1` and the read is at `rax`, 123 ms after the spawn and one demand-paging
fault into the binary. The kernel reported the segfault normally and then the
guest was gone: the harness's next write got `BrokenPipe`, every test behind it
in the shared boot cascaded on the same error, and 147 of 253 reds followed from
one dead QEMU. No kernel panic and no `DOUBLE FAULT` reached the console before
it went.

**What is not established.** Whether the guest died of the fault or of the host
— three concurrent suites is memory pressure QEMU can be killed under, and a
killed QEMU looks exactly like this from the harness. The segfault itself is not
explained either way: it is a userland fault the kernel handled correctly.

**What to read if it happens again.** `err=0x4` is a user-mode read of a
not-present page, so it is `/bin/echo`'s own dereference of 1 and not the
loader's. Two candidates and one run cannot separate them: a `Command::output()`
path whose child inherits something the fault arms disturbed, and demand paging
handing the binary a page it had not finished relocating (`relocs=2600` is on the
trace). Capture the guest with QMP rather than letting the harness kill it —
`BootOptions { qmp: true }` leaves the socket, and `info registers -a` says
whether any other CPU is still running.
