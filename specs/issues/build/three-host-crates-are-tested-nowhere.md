---
status: open
kind: defect
opened: 2026-08-12
---

# Three host-testable crates carry 94 tests that no gate runs

`host-tests.yml`'s `host crates` step loops over fourteen names:

```
toyos-sched toyos-ps2 toyos-gpt toyos-elf toyos-cc toyos-ld toyos-hda
toyos-pci toyos-xhci toyos-desktop kernel-loom kernel-span toyos-fat32
toyos-fat32-check
```

Root `CLAUDE.md`'s "Host tests outside the QEMU suite" list has **sixteen** —
those fourteen plus `toyos-abi/` and `toyos-manifest/`. So two crates the
documentation tells every agent to run are named in no workflow. `cargo test
--lib` at the root does not reach them either: the root package is
`toyos-build`, and `--lib` runs that one lib target.

And `bcachefs/` is in neither list, while being exactly the shape both exist for
— a format library whose whole point is that its decisions are testable without
a guest, and whose kernel adapter (`kernel/src/bcachefs_adapter.rs`, 544 lines)
is guest-only.

Counted 2026-08-12 with `grep -rho '#\[test\]' <crate>/src <crate>/tests`:

| crate | `#[test]` functions | in `CLAUDE.md`'s list | in `host-tests.yml` |
|---|---:|---|---|
| `toyos-abi` | 17 | yes | **no** |
| `toyos-manifest` | 6 | yes | **no** |
| `bcachefs` | 71 | **no** | **no** |
| | **94** | | |

`toyos-manifest`'s round-trip test is the one that makes the build system's
renderer and `/bin/init`'s parser one format — root `CLAUDE.md` says so — and
nothing runs it on a pull request.

**This is the same defect `specs/assessments/ci-plan-assessment-2026-08.md` §5
already recorded once**: four
pure crates were missing from the same loop until 2026-08-08, "so CI was
skipping the cheapest tests it had". It recurred because the list is in two
places and nothing holds them against each other.

The repair is one loop and one gate: put the crate list where both readers take
it from, and have `cargo test --lib` refuse a `CLAUDE.md` name the workflow does
not run. `src/docs.rs` is where such a gate already lives.
