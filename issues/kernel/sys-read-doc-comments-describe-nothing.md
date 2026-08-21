---
status: open
kind: defect
opened: 2026-08-01
---

# `sys_read` blocks: two doc comments that describe code that is not there

Neither changes behaviour; both mislead a reader about an invariant.

**The first citation's file is gone, and not just moved.** `kernel/src/fd.rs:146`
carried `/// Insert at the lowest unused id.` over an `IdMap::insert` call — a
monotonic counter (`let id = self.next; self.next += 1`, `id_map.rs:46-51`)
that never reused a closed fd number, so the comment was wrong the way this
entry describes: lowest-unused is a POSIX guarantee some code may assume, and a
long-lived process leaked fd-number space instead of recycling it. The
capability-endowment rewrite (`6d9d4ac` era) replaced the fd table with
`HandleTable` (`kernel/src/object/handle.rs`), whose `install` (line 244) pops
a freed slot off an explicit free list before growing — the opposite
allocation policy, and it carries no doc comment making either claim. Nothing
here now reproduces this bug; the citation is dead rather than stale.

`kernel/src/process.rs:1194` (was `:958`) — `/// Must run after `teardown_scheduling`, which is
what flushes the child threads' counters into `ProcessData`.` There is no
`teardown_scheduling` anywhere in the kernel. The ordering requirement it states
may still be real; the function that was supposed to establish it is gone, so the
comment names no enforceable precondition.
