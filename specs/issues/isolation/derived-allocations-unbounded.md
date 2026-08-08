---
status: open
kind: defect
opened: 2026-08-01
---

# Derived allocations: one route demonstrated, one unbounded-but-unstaged, one bound

`b554798`. The class is allocations the loader *derives* from inputs, as opposed
to the ones it reads — a per-input ceiling does not constrain a collection fed
from several of them. Three routes were examined and they are **not** equally
established; recording them as one finding would overstate two of them.

- **Route A — demonstrated and fixed.** Two relocation tables of 87,210 entries,
  each individually accepted by `MAX_HEAP_ALLOC`, feeding one index:
  `GlobalAlloc: dlmalloc asked for 2162688 bytes`. A real panic from real input.
- **Route C (`prescan_relocs`) — genuinely unbounded, fixed, NOT staged.** Its
  inputs are `KernelSlice`s over the loaded image and are never gated by
  `MAX_HEAP_ALLOC` at all, so there is no ceiling anywhere on the path. Staging a
  reproducer needs a multi-MiB `.so` whose millions of entries all pass
  `load_shared_lib`'s validation. **Fixed on reading, not on a reproduction** —
  which is the weakest standard this project accepts, and is recorded as such.
- **Route D (`DT_NEEDED` with no `DT_NULL`) — a bound, not a demonstrated
  defect.** It could not be shown to panic: the input ceiling caps that Vec at
  ~1 MiB, so it stays under. Tightened anyway. Do not let it be cited later as a
  fixed vulnerability.

**The fix shape is better than a bound, and is the reusable part: count by type,
then reserve exactly.** That removes growth-by-doubling overshoot — the actual
trigger — and needs no invented number, so there is nothing to justify or
re-derive later. The only explicit ceiling check left is where two
separately-bounded inputs feed one collection, which is exactly the place a bound
on either input cannot help.
