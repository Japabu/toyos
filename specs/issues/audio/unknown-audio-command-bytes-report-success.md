---
status: open
kind: defect
opened: 2026-08-01
---

# Unknown audio device command bytes report success and do nothing

A write to the audio fd carrying a verb the kernel does not implement returns as
if it had worked. A caller cannot tell an accepted command from an ignored one,
which is the shape `SyscallError::InvalidArgument` exists for.
