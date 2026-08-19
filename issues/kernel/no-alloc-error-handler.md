---
status: open
kind: defect
opened: 2026-08-01
---

# `#[alloc_error_handler]` does not exist anywhere in the kernel

Kernel heap exhaustion has no handler. It routes into `try_recover_from_panic`, the path that
frees nothing — so the terminal state of every unbounded-growth entry in this file is an OOM
that cannot report itself cleanly. The three unbounded userland-driven growers under `issues/isolation/` all end
here, which is what makes this worth its own line rather than a clause in each.
