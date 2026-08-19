---
status: open
kind: defect
opened: 2026-08-13
---

# `--worktree add`'s disk refusal message prints half the bound it enforces

`src/worktree.rs`: the refusal check at line 76 compares free space against
the full `NEEDED_BYTES` (25 GiB), but the message it prints divides that same
constant by 2 before formatting it (line 82):

```rust
assert!(
    free >= NEEDED_BYTES,
    "{} has {:.1} GiB free and a worktree's target directories reach about \
     {:.0} GiB.\n...",
    path.parent().unwrap_or(Path::new("/")).display(),
    free as f64 / 1024.0_f64.powi(3),
    NEEDED_BYTES as f64 / 1024.0_f64.powi(3) / 2.0,
);
```

So a refusal that fires below 25 GiB free tells the agent the worktree needs
about 12 GiB — half the real bound. Whoever reads the message and frees
exactly what it asks for hits the same refusal again.
