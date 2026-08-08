---
status: open
kind: defect
opened: 2026-07-30
---

# `SharedToken` is a bare `u32` with no RAII

Unlike `PhysPage`, which cannot leak because Drop returns it to the PMM,
`SharedToken` is `Copy` with no destructor, so the caller must remember to call
the right cleanup function. It should be a non-Copy RAII handle whose Drop
removes the region and frees the backing pages, exposing `.raw()` for the numeric
value to hand to userspace while the owning handle stays in kernel structures.
