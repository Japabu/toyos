---
status: open
kind: finding
opened: 2026-08-09
---

# A Ring 0 instruction fetch at address 0, once, in a process polling two ports

Seen exactly once, on `wt/toyos-endow` during chunk 5's second full suite run
(2026-08-09), and **not reproduced**: the same test alone was green, and two
later full runs of the same tree were green. Filed because a Ring 0 jump to
address 0 is the kernel crashing from userland, which is the one thing it may
never do — so a single sighting is worth the file even without a repro.

## What the machine said

The shared `tests/testcases` boot, during `rs::sched_stress`, which had just
been rewritten from the deleted `SYS_LISTEN`/`SYS_CONNECT` onto ports:

```
[kernel 7.059 cpu0 tid=4] #PF UNHANDLED: cr2=0x0 rip=0x0 err=0x10 user=false tid=Some(Tid(4))
[kernel 7.059 cpu0 tid=4] SEGFAULT tid=4: execute unmapped address at 0x0
[kernel 7.059 cpu0 tid=4]     rbp=0x0000000000000000  rsp=0xffff800008062c50
[kernel 7.059 cpu0 tid=4]     cs=0x0008  ss=0x0010  rflags=0x0000000000010002
[kernel 7.059 cpu0 tid=4]   Backtrace:
[kernel 7.059 cpu0 tid=4]   Stack (from RSP):  … eight quadwords, all zero …
[kernel 7.059 cpu0 tid=4] poison_tid: cpu 0 slot still held 82:0 — its waiter is stranded
```

Three things in that are worth more than the address:

- **`cs=0x0008`, `user=false`** — the kernel, not the process, executed at 0.
- **The whole visible stack is zero and `rbp` is zero**, so this is not a call
  through one bad pointer with a live frame under it.
- **`poison_tid` says pid 82 tid 0 was *already* poisoned and never reaped**, so
  this was the *second* recovery for that process. The first fault's report is
  in the same log with `tid=0`.

pid 82 is `sched_stress` itself: it is the only test binary in that boot with
several threads. QEMU disconnected immediately after, taking 140 collateral reds
with it — the boot, not the test.

## What was running

`sched_stress`'s rewritten first case arms an `io_uring` `POLL_IN` on an
`Acceptor` handle from each of two threads, lets one time out, and **closes the
acceptor handle while its `PendingPoll` is still armed**, then drops the ring.
`Source::Port(Arc<PortShared>)` and `WatcherGuard` are chunk 3's code and had no
in-tree caller before this. The lock order there is `IO_URINGS` then
`PortShared::io_uring_watchers` on the arm path and the same way round on the
drop path, so it is not an obvious inversion — but nothing has audited the case
where the last `Acceptor` handle goes while a poll references the port.

## What would settle it

An arm in `toyos-xhci`-style isolation cannot reach this; it is kernel object
lifetime. The cheap instrument is a guest binary that arms a port poll, closes
the acceptor, and drops the ring, in a loop, on two threads — if the fault is in
that window it will come back inside a boot rather than inside a suite.
