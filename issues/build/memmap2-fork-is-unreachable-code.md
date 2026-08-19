---
status: open
kind: finding
opened: 2026-07-30
---

# The `memmap2` fork is 165 lines of unreachable code

`rust/compiler/rustc_data_structures/src/memmap.rs` cfg-gates
`target_os = "toyos"` to a `Vec<u8>` implementation at all 8 sites, and
`rust/Cargo.toml` is the only manifest that patches memmap2 at all — userland's
duplicate entry resolved to nothing and was deleted 2026-08-01. So no ToyOS code
path calls any memmap2 API. `src/toyos.rs` is compiled and never called; the
fork's only load-bearing content is the `0.9.10 → 0.2.1` version relabel that
satisfies rustc's pin.
Either delete `src/toyos.rs` and let `stub.rs` serve, or drop the toyos gate in
`rustc_data_structures` (the only two APIs rustc uses, `map_copy_read_only` and
`map_anon`, are correct in the fork). Exactly one of the two should exist. Three
real bugs in that module were found and fixed 2026-07-28 — see `forks.toml`.
