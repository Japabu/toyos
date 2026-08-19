---
status: open
kind: defect
opened: 2026-08-17
---

# `SO_CACHE` never evicts, has no bound, and is keyed by a path it never rechecks

`kernel/src/elf/cache.rs:162` is `static SO_CACHE: Lock<Vec<(String, CachedLib)>>`.
The only writer is `cache_loaded_lib`'s `push` (`:206`); the only reader is
`try_clone_cached`'s `position` (`:225`). Nothing removes an entry, nothing
counts them, and `SYS_DLCLOSE` is `arch/syscall.rs:502 => 0` — a literal no-op.
So every distinct path any process has ever `dlopen`ed holds a `PageAlloc` for
the rest of the boot. The 2026-08-15 mechanism-consolidation audit
recorded it in passing — *"a cache with no eviction and no bound … every
other cache got a budget in task #28; this one did not"* — and it was never
filed.

Two things make it worth a file rather than a note.

**The quantity is the workload's and the allocation is contiguous physical.**
`load_shared_lib` (`elf/mod.rs:291`) takes
`PageAlloc::new(align_2m(layout.span()), Category::Elf)`. Measured with
`toyos-elf` against the artifacts this tree has already built
(`rust/build/x86_64-unknown-toyos/stage2/lib/`, built 2026-08-02):
`librustc_driver-ea85c2166f476ad5.so` has a span of 144,760,832 bytes, so one
`dlopen` of it is a **140 MiB contiguous immortal** allocation, plus 8,388,608
bytes of private writable window per process that loads it. The harness gives a
guest 4 GB. 18 `.so` files sit in that directory.

**The key is a path string and the file behind it is never rechecked.**
`try_clone_cached(path)` compares `String`s and returns the cached image without
opening anything. `sys_dlopen` consults it *before* `open_backing`
(`syscall.rs:2562-2565`), and so does `load_needed_libs` (`loader/mod.rs:731`).
`/tmp` and `/home` are writable by any process — `kernel/src/tmpfs.rs:14` exists
so that files under `/tmp` are spawnable and dlopenable — so writing a library,
loading it, rewriting the file and loading it again serves the first image the
second time, with no way for the caller to tell.

Not fixed here: a budget is a policy number and eviction of a shared image needs
a rule for the processes that already have it mapped. Both belong to whoever
owns the loader. The measurement above was taken while scoping the move of the
loader out to Ring 3, 2026-08-17, which would delete this cache rather than
bound it.
