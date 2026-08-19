---
status: open
kind: defect
opened: 2026-08-08
task: 145
---

# Three findings of the second mass-storage audit exist nowhere in VCS

Filed out of the second USB audit entry when the rest of it closed — this is the only trace in the repository that they exist.

F-D, F-F and F-I of the second USB mass-storage audit are recorded **only in
task #145's description**, file and line each. Writing them here from memory
would be inventing them, so whoever holds that task must paste them in.

What is not owed: F-K is `with_storage`'s non-local invariant and is stated at
that function in `kernel/src/drivers/xhci/wait/msc.rs`, F-E is the EP0 recovery
path, and F-J — the file-cache error channel — is closed.
