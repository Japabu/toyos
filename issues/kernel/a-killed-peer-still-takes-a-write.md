---
status: open
kind: defect
opened: 2026-08-20
---

# `kill_while_blocked` reds on `main`'s own tip: a killed peer still takes a write

`tests/toyos-rust-tests/src/bin/kill_while_blocked.rs:178` asserts that a write
to a connection whose peer was killed mid-read is refused:

    assertion `left == right` failed: a connection whose peer was killed
    mid-read still took a write
      left: Ok(22)
     right: Err(NotFound)

It is a race, and it is `main`'s. Same-session A/B, 2026-08-20, dev host,
`cargo test --test toyos-build -- kill_while_blocked`:

| tree | runs | red |
|---|---|---|
| `e4c2c8ff` (`main`'s tip) | 5 | **1** |
| `47892284` (the `toyos-mixer` branch, main merged in) | 5 | 0 |

It also red once inside a full 272-test fast-tier run on the branch, which is
how it was found — a race shows up more often beside 271 other guests than in a
one-test run, so neither count above is its real rate.

The window is between the peer's death and the write: the kernel reaps the read
side, and a writer that raced the reap gets `Ok(22)` — a write into a connection
whose other end is gone — instead of `NotFound`. The guest's own stdout shows
the *pipe* half of the same test getting it right on the same boot: `pipe: the
write end learned its reader had gone`. So the pipe path publishes the death
before the connection path does.

`main` moved twelve commits on 2026-08-20 (`625afce1..e4c2c8ff`), three of them
in the object and memory layers — `kernel/src/object/mod.rs`,
`kernel/src/object/shm.rs`, `kernel/src/mm/region.rs`. Whether the race is new
there or older and merely more likely is not established here; what is
established is that it reproduces on `main` alone.

Not on `src/redlist.rs` — `cargo run -- --known-red kill_while_blocked` answers
`NOT ON THE LIST`, so it has no recorded rate and a landing gate that hits it
has nothing to check the red against.
