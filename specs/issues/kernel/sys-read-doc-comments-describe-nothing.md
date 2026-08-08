---
status: open
kind: defect
opened: 2026-08-01
---

# `sys_read` blocks: two doc comments that describe code that is not there

Neither changes behaviour; both mislead a reader about an invariant.

`kernel/src/fd.rs:146` — `/// Insert at the lowest unused id.` It calls
`IdMap::insert`, which is `let id = self.next; self.next += 1` (`id_map.rs:46-51`):
a monotonic counter that never reuses a closed fd number. Lowest-unused is a
POSIX guarantee some code may assume; this is not it, and a long-lived process
leaks fd-number space rather than recycling it.

`kernel/src/process.rs:958` — `/// Must run after `teardown_scheduling`, which is
what flushes the child threads' counters into `ProcessData`.` There is no
`teardown_scheduling` anywhere in the kernel. The ordering requirement it states
may still be real; the function that was supposed to establish it is gone, so the
comment names no enforceable precondition.
