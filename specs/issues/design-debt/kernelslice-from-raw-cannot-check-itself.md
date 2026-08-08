---
status: open
kind: defect
opened: 2026-07-30
---

# `KernelSlice::from_raw` cannot check the one thing that makes the type safe

`kernel/src/mm/region.rs:16` (the live `TODO`s at `:12` and `:15`). Every bounds
check `KernelSlice` performs is against a size the caller asserted; `from_raw`
cannot validate it against the allocation, so a slice longer than its buffer
passes every check the slice makes. Three call sites, each correct only by
adjacency: `OwnedAlloc::slice` (`process.rs:70`, the one site with an assert),
the ELF loader (`elf/mod.rs`'s `load_shared_lib`, size and allocation share
`load_size` by proximity, not by construction — and every past OOB in the loader
came through this type), and `DmaPool::alloc` (`drivers/mod.rs:34`).

Fix shape: allocators construct the slice. Give `PageAlloc` and the contiguous
PMM path a `slice()` method like `OwnedAlloc`'s, sized from the allocation they
own, then make `from_raw` private to `mm` or delete it. The loader and DmaPool
stop naming sizes at all.
