# Build system

Loads when you read a file under `src/` — the root cargo project, package name `toyos-build`. Root `CLAUDE.md` has `cargo run` and `cargo test`; the `[profile.toyos]` rule is this crate's to enforce (`src/build.rs`).

## Boot modes

- `cargo run -- --gop` boots a UEFI GOP display (`-vga std`) instead of virtio-gpu: the config where the on-screen panic console renders.
- `cargo run -- --diag-boot --build-only` builds `target/bootable-diag.img`, the diagnostic boot for a machine with no serial port: its config declares no `devices`, so the kernel's log stays readable off the panel. Same kernel and bootloader binaries as the ordinary build. **A flashable artifact is built from a committed tree** — `cargo` builds the working tree.
- `cargo run -- --console-boot --build-only` builds `target/bootable-console.img` — `/bin/console`, the shell on the raw framebuffer, for asking a machine questions instead of reflashing it. A third mode, not a replacement for `--diag-boot`.
- `cargo run -- --kernel-param <name>` (repeatable, any boot mode) arms an actuator at boot; every actuator lives in one kernel built with `boot-actuators` (`kernel/src/actuator.rs`), so it decides no kernel build. `--kernel-feature <name>` *is* a build; it accepts any feature `kernel/Cargo.toml` declares, and that set is closed by `cargo test --lib`. An undeclared name is refused by name before any lock.
- `cargo run -- --metal-sim` boots the T14's hardware shape (GOP + NVMe + xHCI + i8042, no virtio, no USB HID, with a 16550); `--mute` takes the serial away, the T14's literal shape.

## Other entry points

- `cargo run -- --check-forks` names every lockfile pin behind the fork branch its manifest consumes. **On demand only: it asks the network**, so it is in neither `cargo test` nor the landing gate.
- `cargo run -- --merge-durations <dir>` merges a sharded CI run's duration files into `tests/test-durations`, refusing anything that is not one whole run; committing the result is the deliberate act.
- **`.github/qemu-version` is the QEMU every guest is measured with, declared once**; `.github/instrument.sh` runs in every gate-workflow job that boots a guest and reds on a disagreement. `check_prerequisites` answers with a `Note:` for this host, and `cargo test --lib` refuses a gate workflow that installs QEMU without naming its instrument (`src/ci.rs`).
- `system.toml` defines which programs to build and the init sequence.

## The host's locks and slots

- `src/buildlock.rs` serialises the stateful phases in two scopes: `Global` (the bootstrap, the sysroot, the rustup link — one directory in `.git/`, shared by every worktree) and `Worktree` (the crate-target cleans). Only `./x.py` typed by hand in `rust/` escapes it. **`rust/library` is the sysroot's fourth source**, witnessed apart (`build/toyos-std-fork-witness`); a worktree finding only a stale std rebuilds it instead of being refused. **Whether you may claim the sysroot is not your choice**: only a checkout whose `toyos-abi`/`toyos` differ from main's may, and that one is told to land; matching main means wait, because the holder landing ends the refusal by itself. **A claim is one only while somebody can still land what it holds** — a holder that has been removed, and a sysroot whose recorded content is already in `origin/main`'s history (the local ref; nothing fetches), are both staleness like the std one, and any checkout whose sources are main's, the primary included, rebuilds over it rather than waiting for a landing that already happened. A claim queues behind every suite run in flight and never lands inside one.
- **The host hands out guest slots and build slots** — `buildlock::guest_slot` (twelve across every worktree, one per task) and `buildlock::build_slot` (four), separate counts so a suite holding every guest slot can still compile. **The order is a constraint at every acquirer**: sysroot → host slot → build lock → artifact. Every blocking lock names its holders and repeats itself every 30 s — a queue is never silence. `cargo test --test toyos-build -- --host-slots N --host-builds N` overrides, 0 turns either off (`--test toyos-build`, not a bare `cargo test` — libtest refuses the flag).

## Worktrees

- Everything under a worktree — targets, images, `.build-locks/` — is its own; the object store, `rust/` and the rustup link are the primary checkout's, and ownership is derived from `git rev-parse --git-common-dir`, never recorded. A linked worktree's `rust/` stays the empty stub `git worktree add` leaves: initialising the submodule clones 913 MiB from GitHub, and git refuses a symlink where a gitlink belongs.
- **Type-checking a std edit without touching the shared sysroot** (seconds, against the rebuild's minutes): point `__CARGO_TESTS_ONLY_SRC_ROOT` at a tree holding an APFS clone of `rust/library` (`cp -Rc`, instant), a workspace `Cargo.toml` naming `library/std`, and symlinks to `toyos-abi`/`toyos`; then `CARGO_TARGET_DIR=<scratch> cargo +toyos build -Z build-std=std,panic_abort --target x86_64-unknown-toyos --offline`. Delete `<scratch>/**/.fingerprint/std-*` between runs — cargo does not re-fingerprint std under `-Zbuild-std` — and it is a compile, not a boot.

## Caveats that bite every agent

- **Documentation carries no gates** — `src/docs.rs` and every test over `specs/` prose were deleted by owner ruling (`8d0db10`); `src/redlist.rs` resolves doc paths only because it gates a Rust table, not a corpus.
- **A red build may be the build system — re-run in isolation before believing any single red.** A `stage1-std/<target>/dist/deps` temp-dir error means a concurrent build, never a broken checkout; never repair or force-rebuild the toolchain. A refusal that your worktree and the shared sysroot disagree about `toyos-abi/src` is correct — the build it stops links against another checkout's struct layouts and no test catches that.
