---
status: open
kind: finding
opened: 2026-08-15
---

# One mapping is written in two ledgers, and only one of them places it

A live `mmap` is recorded twice. `ProcessData::mmap_regions`
(`kernel/src/process.rs`) holds an `MmapRegion` — address, length, and the
`PageAlloc` that owns the physical memory. `AddressSpace::regions`
(`kernel/src/mm/paging.rs`) holds a `Region` at the same address with the same
length — and that one is the source of truth for placement: `find_gap` reads it
and nothing else.

The FIXED arm of `sys_mmap` wrote the first and not the second, and the
consequence was a kernel panic three ordinary syscalls from any C program that
passes `MAP_FIXED` — the placement search handed the next anonymous request the
range a live FIXED mapping was already in, and `map_range` asserted on a present
PDE. That is fixed: both ledgers now move together in `sys_mmap` and in
`sys_munmap`, every mapping is registered where the placement search looks, and
`munmap`'s `expect` names the invariant. This entry is the shape that let it
happen, which the fix does not remove.

**Nothing checks that the two agree.** They are written by the same two
functions and by nobody else, so agreement is a property of those two functions
being read carefully. A third writer — or a fourth arm of `sys_mmap` — reopens
exactly the same defect, and no test would see it until a placement collided.

**What a consolidation looks like.** `Region` gains the physical ownership that
`MmapRegion._pages` carries, `mmap_regions` is deleted, and the address space is
the only ledger. What that has to answer first:

- **Accounting.** `alloc_count`, `free_count` and `peak_memory` are summed over
  `mmap_regions` at `kernel/src/arch/syscall.rs`, and `SYS_SYSINFO`'s per-process
  memory line sums `_pages` over it under a `try_lock` on the process data —
  a `try_lock` the crash report depends on, so that reader may not move to a
  lock it can block on.
- **Teardown.** `mmap_regions.clear()` in `kernel/src/process.rs` frees the
  pages at a point in process teardown chosen relative to the address space's
  own drop; folding them together makes that one ordering instead of two.
- **`Unmapped`.** The pages of a freed mapping may not reach the PMM until
  every CPU has flushed, and the value carrying them has to leave the locks
  before it drops (`kernel/src/mm/unmapped.rs`). A region map that owns pages
  has to hand them out of `&mut self` for that, which is the shape
  `shared_memory` already uses and the reason this is not a mechanical move.

Not urgent: with both writers correct the ledgers agree, and the panic that
found this is gated by `mmap_stress`. Filed because the fix that deletes the
second ledger is better than the fix that keeps them in step, and because the
next person to add a placement path should find this before writing it.
