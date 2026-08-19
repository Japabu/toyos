---
status: open
kind: defect
opened: 2026-08-19
---

# Nothing in this repository has ever run clippy, including on the kernel, where it works today

`src/sourcegate.rs`'s header says it plainly: *"**Nothing in this repository runs
clippy** — not CI, not `cargo test`, not the build — so a `clippy.toml` would be
a wall with nothing behind it."* That is still true.

The reason usually given for the kernel — that the `toyos` toolchain ships only
cargo and rustc — is **wrong, and was wrong when it was given.**
`kernel/.cargo/config.toml` sets `target = "x86_64-unknown-none"`, a standard
built-in Rust target. So

```
rustup run stable cargo clippy --target x86_64-unknown-none
```

lints the kernel with the stable toolchain, no fork work and no new toolchain.
Only *userland* needs the fork, because `x86_64-unknown-toyos` is the custom
target. Cost of running it on the kernel today: nothing. It has always been
available and nobody tried.

## Measured 2026-08-19

| | findings |
|---|---:|
| default clippy, kernel | **61** |
| default clippy, host workspace | **~70** |
| **default, both** | **~131** |
| **plus `pedantic` + `nursery`, kernel alone** | **1,684** |

The default run already names a real bug class: **`named constant with interior
mutability`, twice.** A `const` holding an atomic is copied at each use site, so
writes go nowhere and the code reads as correct. Also `this operation will
always return zero`, four `this operation has no effect`, and two
operator-precedence findings in xHCI DMA address arithmetic
(`dma.phys() + OFF_CMD_RING as u64 | 1`).

`pedantic` + `nursery` is a **27× jump** and mostly taste —
`must_use_candidate`, `module_name_repetitions`, `cast_possible_truncation`,
which in a kernel fires constantly and is usually deliberate.

## Shape

**Default clippy, denied, on every PR** — kernel, host workspace and bootloader.
About 131 findings to clear, each surviving `allow` carrying its reason at the
site. A local hook so it reds before CI does.

**`pedantic` and `nursery` adopted one lint at a time**, each one's finding count
measured before adoption. A lint that costs hundreds of mechanical edits and
catches nothing is refused **by name**, recorded so nobody proposes it again.
Turning the groups on wholesale has one predictable outcome — a blanket `allow`
to make CI pass — which is worse than not turning them on.

**`clippy::restriction` is never enabled as a group.** It is documented as
containing lints that contradict each other by design. Cherry-picked only.

## This supersedes the hand-built unsafe gate

The unsafe track — every `unsafe` block stating why it is needed and why it is
sound — was scoped as a fourth scan in `src/sourcegate.rs`. It does not need to
be. `clippy::undocumented_unsafe_blocks` is that lint, it lives in
`restriction`, and it is one line of configuration adopted per area as each
area's justifications land. The scan should not be written.
