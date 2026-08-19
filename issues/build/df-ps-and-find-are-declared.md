---
status: open
kind: defect
opened: 2026-08-08
---

# `df`, `ps` and `find` are external binaries the bar does not allow

Three more, none of which comes with Rust or QEMU.

All three are in the preflight's `ALSO_USED` list since 2026-08-08, so a machine
without one is told which and what it costs. **That is a declaration and not the
fix** — each is still called, and each replacement is still cheap: `ps` has the
answer beside it in the same file (`getloadavg` through the `libc` crate),
`find`'s job is a directory walk, `df`'s is one syscall.

- `src/worktree.rs:141` — `df -k`, `.expect("run df")`, so `worktree add` dies
  without it. One free-space number.
- `tests/common/hostload.rs:111` — `ps -Ao comm=` for gate A's `host:` line.
  Degrades correctly (`.ok()?`), so its absence costs a diagnostic and not a run.
  Its neighbour `getloadavg` is already reached through the `libc` crate, which
  is the shape this one wants.
- `toyos-fat32/tests/common/mod.rs:301` — `find <mount> -name '._*' -delete`,
  sweeping macOS resource forks off a freshly-populated volume. **Not on the
  known list of that file's macOS tools**, and it belongs there: it is reached by
  the same fixtures as `newfs_msdos`/`hdiutil`/`fsck_msdos`, which is all 59
  `#[test]`s in `toyos-fat32/tests/`.

Reach of the three FAT tools, since "nine tests" understates it:
`fsck_complaints` is reached by five `MACHINE_TESTS` entries (`esp_filesystem`,
`toybox_cp_volume`, `kernel_log_file`, `log_partition_layout`,
`log_partition_identity`), `src/image.rs`'s own `fsck` by two `#[test]`s that run
under `cargo test --lib` — which is in the landing gate — and
`toyos-fat32/tests/common/mod.rs` by all 59.
