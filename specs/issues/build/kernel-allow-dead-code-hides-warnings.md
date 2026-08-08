---
status: open
kind: defect
opened: 2026-08-08
---

# The kernel's crate-level `allow(dead_code)` hides 49 warnings from the zero-warning bar

`kernel/src/main.rs:3` is `#![allow(dead_code)]`. `kernel/.cargo/config.toml`
carries `-Dwarnings`, so the kernel is warning-clean today *because of* that one
line rather than in spite of it.

Measured by reproducing `src/build.rs:209-256`'s invocation as a `cargo check`
into a scratch `CARGO_TARGET_DIR`, so neither the worktree nor the shared sysroot
was touched:

```
cd kernel && RUSTUP_TOOLCHAIN=toyos \
  RUSTFLAGS='--force-warn dead_code -Cforce-frame-pointers=yes' \
  CARGO_TARGET_DIR=<scratch> cargo check --target x86_64-unknown-none \
  --profile toyos --message-format=json
```

**71 `dead_code` messages, 65 of them the kernel's** (3 `bcachefs`, 3
`hashbrown`); the same command without `--force-warn` gives 0. `--force-warn`
overrides item-level allows too, and four of those exist in the kernel
(`arch/mod.rs:3` on `mod debug`, `main.rs:42` on `mod vfs`, `id_map.rs:92`,
`drivers/virtio.rs:73`), accounting for 16 of the 65. **So deleting `main.rs:3`
alone surfaces 49.** They are unused constants, never-read fields and unreachable
methods — `PORTSC_CCS/PED/PR/SPEED`, `CMD_GET_DISPLAY_INFO`, `DR7_*`,
`struct DisplayOne`, `static NEXT_CPU_ID`, `map_alloc`, `is_mapped`,
`clear_regions`, `unmap_2m`, `wake_one`, `port_bit`, `redirection`. Default
features only; an actuator feature could move the number.

Same family as *The fork estate is invisible to the zero-warning bar* below: a
bar with a crate-level exemption under it certifies less than it looks like it
does, and "Dead code is deleted" is a stated principle.
