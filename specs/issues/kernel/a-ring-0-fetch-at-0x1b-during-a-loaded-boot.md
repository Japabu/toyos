---
status: open
kind: defect
opened: 2026-08-15
---

# A Ring 0 instruction fetch at `0x1b`, in a daemon's boot, under a wide suite

Second sighting of the class
`specs/issues/kernel/ring0-jump-to-zero-under-port-polls.md` names, at a
different site and with a different address. Filed apart because that file's
workload — two threads polling ports, a poisoned thread already on the floor —
is not this one: this is an ordinary boot, 0.372 s in, before the test-runner
existed, with nothing in the log before it.

Seen once, dev host, `cargo test` twelve wide with 78 guests, on
`wt/toyos-mapfixed` (kernel delta: `sys_mmap`'s FIXED arm). `ALONE: GREEN`, and
the same kernel booted 76 guests green in the run immediately before.

```
FAIL esp_filesystem: [qemu] Init process crashed during boot:
[kernel 0.372 cpu1] SEGFAULT tid=0: execute unmapped address at 0x1b
[kernel 0.372 cpu1]   rip:
[kernel 0.372 cpu1]     0x1b
[kernel 0.372 cpu1]   Page walk for 0x1b [PML4=0x264d000 PCID=2 PML4[0] PDPT[0] PD[0] PT[0]]:
[kernel 0.372 cpu1]     PML4E: 0x0000000000000000 P=0 W=0 U=0
[kernel 0.372 cpu1]     rax=0x000000ffff5fff90  rbx=0x0000000000000023
[kernel 0.372 cpu1]     rbp=0x0000000000010246  rsp=0xffff800002673000
[kernel 0.372 cpu1]     cs=0x0008  ss=0x0010  rflags=0x0000000000257842
[kernel 0.372 cpu1]   Backtrace:
[kernel 0.372 cpu1]   Stack (from RSP):
[kernel 0.372 cpu1]     [0xffff800002673000] = 0x0000000000000000
[kernel 0.372 cpu1]     [0xffff800002673008] = 0x000000000000c943
[kernel 0.372 cpu1]     [0xffff800002673010] = 0x000000000016e000
[kernel 0.372 cpu1]     [0xffff800002673018] = 0x000001000008d7c0
```

## What the registers say, and what they do not

**It is the kernel executing, not the process**: `cs=0x0008`, `ss=0x0010`, and
`rsp` in the direct map. The report calls it `SEGFAULT tid=0` because that is
what the fault handler says about a user thread; the ring is what matters.

**`rbp=0x10246` is not a frame pointer, it is `RFLAGS`.** `0x246` is the value
every ordinary `pushfq` produces here and `0x10000` is `RF`, set on a fault —
which is exactly what the predecessor's audit identified as `context_switch`
restoring a frame that no longer means what the frame layout says
(`alloc_kernel_stack`, `kernel/src/loader/start.rs`: `rbp` at slot 5, `RFLAGS`
at slot 6, the return address at slot 7). Here `rbp` holds slot 6's value and
`rip` holds a word from beyond it, so the frame is *shifted*, not zeroed —
which is the one thing that differs from the earlier sighting, whose stack was
entirely zeros because the page had been through `alloc_zeroed`. A shifted frame
is not a reissued stack; it is a frame written or restored at the wrong offset.

**The stack under the bad frame is not empty.** `rsp` is at a 4 KiB boundary and
the words above it carry live-looking values, two of them user addresses
(`0x1000008d7c0`, `0x10000001104`) — where the earlier sighting reported eight
zeros. Whatever this frame is, it is not a page that came back from
`alloc_zeroed`.

## What is not the cause

`esp_filesystem` boots an ESP configuration and does no `mmap` with
`MmapFlags::FIXED`; the branch it was seen on changes only that arm of
`sys_mmap` plus a `munmap` that is line-for-line the same operation it was
(`unmap_range` + `free_region` became `free_and_unmap`). The same kernel ran the
full suite green on this test immediately before and passed it `ALONE`
immediately after.

## What to do with it

Not reproducible on demand, so nothing here is a repro recipe. The value is the
register state: **the next sighting should be compared against this one on
whether the frame is zeroed or shifted**, because those are two different
mechanisms and the tree now has one instance of each. `specs/issues/kernel/poison-set-holds-one-thread-per-cpu.md`
is the other open thread the first sighting pulled on.

Do not close this on green runs.
