---
status: assigned
kind: defect
opened: 2026-08-01
---

# Two syscalls discard a failure signal they already have

`sys_mkdir` calls `vfs.create_dir(&resolved)` and returns `0` unconditionally
(`syscall.rs:1424-1430`). `sys_connect` calls `listener::push_connection(..)`,
which returns `bool`, as a bare statement (`syscall.rs:1042`; `listener.rs:97`).

Filed as one entry because the pattern is the finding, not either instance:
**a bound is only as good as the caller's willingness to hear "no".** In both
cases the underlying operation can already refuse, and the syscall layer throws
the answer away — which is exactly why neither can be given a bound today
without the bound becoming a silent failure.

It is the direct counterpart of `client-request-is-an-allocation`. There, a
client's request is an allocation request that needs an owner who can say no.
Here the owner *does* say
no and nobody is listening, so adding the cap without fixing the caller would
convert an unbounded resource into a silently dropped request — a worse failure,
because the first is at least visible.
