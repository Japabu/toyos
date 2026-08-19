---
status: assigned
kind: defect
opened: 2026-08-01
---

# soundd never frees the per-client shm region

`SharedMemory::Drop` only unmaps and nothing calls `destroy()`, so each
open/close cycle strands a 2 MiB page. **Bounded by the next process exit, not by
soundd's lifetime** — `cleanup_process` sweeps every region owned by an exiting
process — so this is a real leak with no release at close time, but not a
permanent one. An earlier filing claimed the page was stranded for soundd's whole
run, which overstates it: a long-lived soundd accumulates only until whichever
process owns the region exits.

**ASSIGNED** to the isolation agent, merged with `SYS_GRANT_SHARED`'s missing
revocation: revoke and reclaim are one mechanism, and fixing either alone leaves
the other holding the same page.
