---
status: open
kind: defect
opened: 2026-08-19
---

# The build system does not compile on Windows, and the one call that stops it is in the toolchain bootstrap

`src/toolchain.rs:1788`, inside `link_host_target`:

```rust
std::os::unix::fs::symlink(&source, &host_target_dir).unwrap_or_else(|e| {
```

No `cfg` guard. `std::os::unix` does not exist on Windows, so this is a compile
error rather than a runtime failure — the build system cannot be built there at
all, and the failure arrives at the least useful moment, before anything can
report a better message.

The only `#[cfg(unix)]` in the whole build system is one line in
`src/ci.rs:206`. Nothing else branches on platform. (Verified 2026-08-19:
`grep -n "cfg(unix)\|cfg(windows)\|cfg(target_os" src/*.rs` returns that line
and two `cfg(target_os = "toyos")` strings that are `Cargo.toml` text the
toolchain writes, not conditional compilation of this crate.)

**It is not in a corner.** `link_host_target` gives the ToyOS sysroot the host
target that proc-macros compile against — `host_target_missing` calls it during
toolchain setup. It is on the path everyone takes on a first build.

## Why this is filed now

The shared-target-directory work
(`issues/build/every-worktree-builds-its-own-copy-of-the-same-crates.md`)
was designed to be portable by construction — a path join, no platform branch
anywhere — on the stated requirement that this project compiles on every major
OS. That requirement is not met today, so the new work would be a portable
component inside a build system with an unconditional Unix dependency at its
centre. Worth knowing before the portability of anything else is claimed.

## The self-hosting question underneath

The north star is that nothing rests on a host binary and that everything can
eventually run inside ToyOS. `symlink` is the question in miniature: either
ToyOS grows symbolic links, or `link_host_target` needs a shape that does not
need one — a copy, a directory junction, or a sysroot layout that does not
require aliasing a directory at all.

Deciding that is worth more than a `#[cfg]` pair, and a `#[cfg]` pair would
close the Windows hole while leaving the ToyOS one open. Both readings should be
taken together rather than the cheap half being applied on its own.

## Unverified

Whether this is the *only* thing stopping a Windows build. It is the only
`std::os::unix` reference in `src/`, but nothing has attempted the build —
process spawning, path handling and the `df` call in `src/worktree.rs:141`
(`Command::new("df")`) are the obvious next candidates and none has been
checked. `df` is a Unix tool and would fail at runtime rather than compile time,
which is a different and quieter kind of broken.
