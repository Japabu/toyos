---
status: open
kind: finding
opened: 2026-08-10
---

# Thirteen machines build the same tree, and it is the largest item left on CI's critical path

Each `ci.yml` guest shard runs `cargo test --test toyos-build`, which compiles
the kernel, the bootloader, every userland program and the harness before it
boots anything. Every shard compiles the same tree, from nothing, on its own
machine.

Measured off run `31330069053` (`main` at `cc2ebf1`, twelve green shards),
shard 10 — the widest:

| | |
|---|---|
| job, end to end | 1,058 s |
| `deps` (`apt-get` in the container) | 56 s |
| `install the toolchain` | 5 s |
| `suite` step | 976 s |
| of which the harness's own total | 540.8 s |
| **so `cargo build`** | **≈ 435 s** |

435 s × 13 jobs is about 94 minutes of runner time per run, and 435 s of the
1,058 s critical path — **larger than the 143 s the duration-profile drift was
costing** (`specs/ci-plan.md` §11.2), which was the item this was found beside.

The obvious lever is a restored `target/`. `host-tests.yml` and `landing.yml`
already use `actions/cache` that way, and a cache written on `main` is the only
one every branch can restore (§8.4). What makes it more than a one-line change:

- Twelve shards start together, so none of them benefits from a sibling's save
  within a run — the win is across runs, and a branch's *first* run only gets
  what `main` last wrote.
- Only one job should write the key, or twelve concurrent uploads of the same
  content race for it.
- The guest build's fingerprints include the sysroot path
  (`$PWD/rust/build/…/stage2`), which is stable on a runner and is *not* stable
  between a runner and anywhere else — so the key has to be the toolchain tag as
  well as the lockfile.
- A `target/` for this tree is large, and `specs/ci-plan.md` §7.3 records what
  an unbounded upload did to shard 9 of run `31247206462`: 44 minutes, and a job
  that had its answer reported as cancelled.

None of that is an argument against it; it is the reason it wants a measurement
of its own rather than being folded into somebody else's task. The number to
beat is 435 s, and the arm that settles it is one shard with a warm cache
against one without, in the same run.

A second, smaller item sits beside it: the `deps` step is 52–84 s of `apt-get`
for four packages, thirteen times per run. A prebuilt image on ghcr.io replaces
it with a pull. That trades an `apt-get` for a registry — a dependency question
as much as a speed one — and it is 5% of the critical path against the build's
41%.
