---
status: open
kind: defect
opened: 2026-08-07
---

# `toyos-ld`'s alloc-shim table names a compiler hash that no longer exists

`ALLOC_SHIMS` and `SHIM_NO_ALLOC_UNSTABLE` in `toyos-ld/src/collect.rs` are
eleven string literals of the form
`_RNvCs2fcwfXhWpkc_7___rustc12___rust_alloc`, where `Cs2fcwfXhWpkc` is a rustc
crate disambiguator. Measured 2026-08-07: that spelling occurs **zero** times
across the 30 rlibs of the `x86_64-unknown-toyos` sysroot, and the live
disambiguator is `CshVjSbrpHdcL` — 4 occurrences in `liballoc`, 5 in
`libpanic_abort`, and so on. So `synthesize_alloc_shims` currently synthesizes
nothing, and would again the next time `rust/` is rebuilt.

Inert today, because rustc emits the allocator shim into the leaf crate's own
object for a real binary; the table only matters for a link that has rlibs and
no leaf crate. Found while assembling a real corpus for the determinism gate,
which is exactly such a link and which failed on those six names.

The defect is the shape rather than the staleness: a compiler-internal hash
frozen into a string literal has no way to announce that it has gone stale, and
the symptom when it does is an undefined symbol far from the cause. Matching on
the `___rustc` path and the function name, with the disambiguator wild, would
not need updating.
