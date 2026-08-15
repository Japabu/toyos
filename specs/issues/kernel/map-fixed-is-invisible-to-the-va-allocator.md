---
status: open
kind: defect
opened: 2026-08-15
---

# A `MAP_FIXED` mapping is invisible to `find_gap`, so the next `mmap` panics the kernel

There are two records of which user virtual ranges are taken, and the fixed
branch of `sys_mmap` writes to only one of them.

`AddressSpace.regions` is the source of truth for the allocator:
`PageTables::find_gap` (`kernel/src/mm/paging.rs:468`) searches nothing else,
and both allocating entry points — `alloc_region` (`:497`) and `alloc_and_map`
(`:517`) — insert into it. Every other mapping path registers there too: ELF
segments and the stack (`kernel/src/loader/mod.rs`), shared memory
(`kernel/src/object/shm.rs`), and the non-fixed arm of `sys_mmap`
(`kernel/src/arch/syscall.rs:2140-2141`).

`ProcessData.mmap_regions` is a second list of the same ranges, used for
`munmap` and for the peak-memory figure.

The fixed arm (`kernel/src/arch/syscall.rs:2110-2135`) maps with
`PageTables::remap` (`:2117`) and records **only** in `mmap_regions` (`:2128`).
It never calls `insert_region`. `remap` (`kernel/src/mm/paging.rs:352`) asserts
alignment and nothing else — it replaces a PDE without consulting `regions` and
without adding to it.

So after a successful `MAP_FIXED` at address A, `A` is not in `regions`, and
`find_gap` may hand the same range to the next anonymous `mmap`. That reaches
`alloc_and_map` → `map_range`, whose assert fires:

```rust
// kernel/src/mm/paging.rs:332-335
assert!(
    existing & PAGE_PRESENT == 0,
    "map_range: PDE already present at vaddr {va:#x} (existing={existing:#x})"
);
```

## Reachability

Ordinary C. `userland/libc/src/posix_io.rs:441` maps `MAP_FIXED` (0x10) onto
`MmapFlags::FIXED` and passes it through.

`fixed_start` is validated (`kernel/src/arch/syscall.rs:2083-2098`) for 2 MiB
alignment, for `alloc_floor()`, for `ALLOC_CEILING` and for the user half — and
that validation is careful and well argued. It is not checked against `regions`,
which is the one check that would matter here. Because the accepted range is
bounded by exactly the window `find_gap` searches, the collision is not a corner
case: a fixed mapping placed just under the topmost live region is handed back by
the very next anonymous `mmap`.

This violates CLAUDE.md's *"the kernel never crashes from userland"*, and the
assert is a kernel-bug fail-fast reached from a syscall argument, which is the
shape `specs/capability-endowment-spec.md` refuses.

## Why no gate catches it

`tests/toyos-rust-tests/src/bin/mmap_stress.rs:72-79` makes one fixed mapping,
writes to it, and `munmap`s it at `:79` before doing anything else — it never
holds a live fixed region across another `mmap`.
`tests/toyos-rust-tests/src/bin/tlb_shootdown_waits.rs:121` maps fixed *over an
address a previous non-fixed `mmap` returned*, so that range is already in
`regions` and the divergence cannot show.

## What a fix has to decide

The fixed arm should register in `regions` like every other path. But
`insert_region` (`kernel/src/mm/paging.rs:556`) asserts the address is
unoccupied, and here the address came from userland — so on this path the
overlap must be a **refusal**, not an assert. Deciding *which* refusal is the
real work: a fixed request over a live mapping is either `InvalidArgument`
(the caller named an occupied range) or a deliberate replace, and POSIX's answer
(silently replace) is the one this kernel has the least reason to copy.

Once `regions` carries fixed mappings, `ProcessData.mmap_regions` has no
information `regions` lacks, and `vma::Region` growing an owned `PageAlloc`
would let the second list go entirely — about 30 lines, and one fewer pair of
records that can disagree.

## Confidence

Traced through every registration site and both asserts; the line numbers here
were re-read at `71a0559`. **Not executed** — this was found in a read-only
audit, so the panic is derived from the code rather than observed. Reproducing it
is a two-call C program and should be the first step of any fix.

Adjacent, not the same: `specs/issues/hardware/anonymous-mmap-is-not-demand-paged.md`
covers `sys_mmap`'s eager allocation.
