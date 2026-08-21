---
status: open
kind: track
opened: 2026-08-20
---

# The toolchain ships no cargo, and the shared cache waits on one

**The owner ruled on 2026-08-20**: the shared build cache lands on our own
toolchain carrying its own cargo — "we use our own toolchain anyways" — rather
than waiting for upstream stabilization or riding the machine's cargo.

**And ruled again the same day: postponed.** Shipping our own cargo is a
major ecosystem milestone with a lot of dependencies, and it deserves its own
era — this track is the destination, not current work, and nothing is staffed
on it until the owner opens that era. Until then the practice stands:
per-worktree targets, and merged worktrees removed promptly.

**The gap this closes.** The `toyos` toolchain today is our rustc and std;
its `cargo` is a symlink to whatever the machine has — a rustup nightly if one
exists, the host's stable otherwise (`toolchain::host_cargo`), and on every CI
runner that means stable. The measured shared-cache design
(`issues/build/every-worktree-builds-its-own-copy-of-the-same-crates.md`) is
sound under cargo's `checksum-freshness` mode and silently catastrophic
without it — and stable cargo ignores the mode's config switch *silently*,
which is the exact hazard shape: sharing on, safety off, no diagnostic.

**The shape of the work.**

1. The toolchain build (`rust/`'s dist, driven from `src/toolchain.rs`)
   builds and ships cargo in the same artifact as rustc and std, on the
   fork's own channel, so `-Z`/`[unstable]` switches are honored natively
   and identically on every machine.
2. The build system and the generated worktree config invoke that cargo and
   no other; a machine without the toolchain fails loudly by construction —
   there is no stable-cargo path left to fall back to silently.
3. Then, and only then, the shared target directory from the measured design:
   `<primary>/target` derived from the common dir, the bypass sites routed
   through `hostws::target_dir`, `checksum-freshness` on in the worktree
   config, and the mis-link experiment from the refutation re-run as the
   regression proof.
4. The costs to measure, not guess: the artifact's size growth, the
   bootstrap-time delta, and whether CI's toolchain cache keys need the
   cargo half.

Self-hosting is the north star and this is a step toward it: one more piece
of the build that rests on the host today and on our own tree after.
