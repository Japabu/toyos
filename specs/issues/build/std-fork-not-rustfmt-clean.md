---
status: open
kind: defect
opened: 2026-08-11
---

# The std fork's ToyOS files are not rustfmt-clean, and upstream requires that they are

`x fmt --check` is a hard gate on `library/` upstream, so this is the first thing
an upstream PR of the fork delta would be told about — and the fork's promise is
that its delta *is* that PR.

Measured 2026-08-11 against the fork's own `rustfmt.toml` (`style_edition =
"2024"`, `use_small_heuristics = "Max"`) with `rustfmt 1.9.0-nightly
(d9563937fa 2026-03-03)`:

```
for f in $(git ls-files 'library/**/*toyos*'); do
  rustfmt +nightly --config-path rustfmt.toml --edition 2024 --check \
    --unstable-features --skip-children --color=never "$f"
done
```

Fourteen of the twenty-one `*toyos*`-pathed files under `library/` drift, sixty
hunks in all. The two largest are `sys/net/connection/toyos.rs` (15) and
`sys/fs/toyos.rs` (14). The cross-platform files the fork touches — the
dispatch arms, which nobody wrote freehand — are clean, `sys/process/mod.rs`
included.

Nothing in the ToyOS repository runs `x fmt`, so the debt is invisible from
here. It cannot be paid inside a defect fix without burying the fix: formatting
`sys/fs/toyos.rs` rewrites most of the file. It wants a commit of its own, which
is cheap now that a `rust/library` edit is picked up by an ordinary worktree
build (`specs/worktrees.md` §3.3) and needs no sysroot claim.
