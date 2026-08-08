---
status: open
kind: rejected
opened: 2026-08-01
---

# Bounds that are policy, not format

Each is a number this crate picked, with its derivation at the definition:
`MAX_DIR_ENTRIES` (65,536 entries, 2 MiB, ~20k files in one directory);
`MAX_WALK_DEPTH` (32); `MAX_SHORT_NAME_CANDIDATES` (64, after which a create
into a directory built to collide returns `NoSpace`); `MAX_LFN_CHARS` (255,
this one *is* the format). A `walk` or `read_dir` past the caller's `limit`
refuses rather than truncating, for the reason `vfs::MAX_LIST_ENTRIES` gives.
