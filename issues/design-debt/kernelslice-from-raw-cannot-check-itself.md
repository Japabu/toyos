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

**One call site left, and it is correct only by adjacency**: the ELF loader
(`elf/mod.rs`'s `load_shared_lib`, where size and allocation share `load_size` by
proximity rather than by construction — and every past OOB in the loader came
through this type).

Fix shape for what is left: the allocator constructs the slice, sized from the
allocation it owns, and then `from_raw` is private to `mm` or deleted. The loader
stops naming sizes at all.

## 2026-08-22 — `PageAlloc` has one now

`PageAlloc::window()` (`process.rs`) is that method, sized from the allocation's
own `ptr()`/`size()`, and the demand-paging fill and `UserStack` reach the frame
through it instead of through a bare `*mut u8`. `OwnedAlloc::slice` is the other
one that already had it, with an assert.

## 2026-08-22 — DMA memory no longer goes through this type at all

`DmaPool::alloc` was the third unchecked call site. DMA memory is `mm::Dma` now:
the pool constructs the view, the constructor is private to `mm::dma`, and the
view **borrows the pool** — so for that memory the type does check that the
region outlives it, and `DmaPool::leak` is the only way to reach `'static`.
`KernelSlice::ptr_at`, whose `offset <= size` bound said nothing about the length
read through the pointer it returned, had no callers left and is deleted with it —
so the offset-only bound is out of the tree entirely. The loader's `load_size` is
what keeps this open.
