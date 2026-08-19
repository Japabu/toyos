---
status: open
kind: defect
opened: 2026-07-31
---

# `sys_read` blocks on an empty Keyboard fd and returns `NotFound` on an empty Mouse fd

Two fds of the same shape, two different answers to the same question. Pick one.

The spurious-readiness half of this entry is closed: `handle_report` returns the
number of events it queued and `dispatch_report` wakes only on a non-zero count,
so readiness and `has_data()` agree. Userland still reads both fds non-blocking,
which is now belt and braces rather than a workaround.

**The other half, not previously recorded: two of the wait queues are woken by
nobody's benefit.** `sched::waitqs::MOUSE` and `sched::waitqs::NETWORK` each
appear at exactly one site in the kernel — their own wake (`mouse.rs:154`,
`net.rs:54`). Nothing ever parks on either. The wakes are real calls on a hot
path doing nothing, and they are the direct consequence of the asymmetry above:
because `sys_read` returns `NotFound` on an empty Mouse fd rather than blocking,
there is never a parked mouse reader to wake. Fixing the asymmetry by making
Mouse block is what would give `MOUSE` a waiter; deleting the queues is what
would make the current behaviour honest. Do not do neither.
