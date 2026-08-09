---
status: open
kind: finding
opened: 2026-07-30
---

# Profiling layers 2 and 3 are not built

Layer 1 (process accounting counters + the `stats` tool) is implemented, with
`process-stats-exited-child-only`'s read-path restriction. Event tracing and RIP
sampling are not. See
CLAUDE.md's diagnostics roadmap.
