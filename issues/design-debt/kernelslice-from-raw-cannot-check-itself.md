---
status: open
kind: defect
opened: 2026-07-30
---

# `KernelSlice::from_raw` cannot check the one thing that makes the type safe

`kernel/src/mm/region.rs` (the live `TODO`s on the type and on `from_raw`). Every
bounds check `KernelSlice` performs is against a size the caller asserted;
`from_raw` cannot validate it against the allocation, so a slice longer than its
buffer passes every check the slice makes.

**Two call sites, each correct only by adjacency**: `OwnedAlloc::slice`
(`process.rs`, the one site with an assert) and the ELF loader (`elf/mod.rs`'s
`load_shared_lib`, where size and allocation share `load_size` by proximity
rather than by construction — and every past OOB in the loader came through this
type).

**The third is gone (2026-08-22).** `DmaPool::alloc` was the other one, and DMA
memory is `mm::Dma` now: the pool constructs the view, the constructor is private
to `mm::dma`, and the view borrows the pool — so for that memory the type *does*
check that the region outlives it, and `DmaPool::leak` is the only way to reach
`'static`. `KernelSlice::ptr_at`, whose `offset <= size` bound said nothing about
the length read through the pointer, had no callers left and is deleted with it;
`copy_from` went the same way.

Fix shape for what is left: allocators construct the slice. Give `PageAlloc` and
the contiguous PMM path a `slice()` method like `OwnedAlloc`'s, sized from the
allocation they own, then make `from_raw` private to `mm` or delete it. The
loader stops naming sizes at all.
