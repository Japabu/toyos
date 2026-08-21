---
status: open
kind: track
opened: 2026-07-27
---

# toyos-cc has never compiled TinyCC, which is the first step of self-hosting

toyos-cc is exercised against TinyCC's own test corpus — 155 cases compiled for
ToyOS and executed in the guest by `cargo test`. That covers "toyos-cc handles
TinyCC's C". It does not cover "toyos-cc builds the TinyCC compiler", which is
the step the north star actually needs.

A crate for it existed and was deleted 2026-08-01: nothing built it, and it
targeted the **host** — Mach-O, `-e _main`, macOS SDK headers, a panic on any
other OS — so its output could never have run on ToyOS. It also failed at stage
1 inside Apple's `__darwin_mcontext64`, and repairing that meant teaching
toyos-cc macOS header internals, which buys ToyOS nothing and contradicts
toyos-cc not being meant to grow.

**Whoever restarts this: target ToyOS against our own libc from the first line,
and pin a stable release tarball with a verified hash rather than a snapshot.**

The chain past TCC, priced once so it is not re-derived: autoconf needs a POSIX
shell (dash, ~15k lines of C), which needs GNU make (~30k), plus `ar`/`ranlib`/
`nm`, plus sed/awk/grep, plus a coreutils subset, plus flex and bison unless a
release tarball ships pre-generated parser output. Several of those have Rust
implementations that would replace the C entirely, and that is the cheaper road.
Whether the destination is GCC at all is an open question — it sits awkwardly
against the no-LLVM, Cranelift-backend direction.
