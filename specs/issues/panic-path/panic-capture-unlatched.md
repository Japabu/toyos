---
status: open
kind: defect
opened: 2026-07-31
---

# `capture()` is unlatched, so two simultaneous panics interleave the snapshot

`kernel/src/drivers/panic_console/mod.rs:395-402`. Both panicking CPUs take
`cli` first (`main.rs:102`), so neither takes the other's halt IPI, and both
`peek_tail` into the same static. Harmless in itself — same ring, `len` read
once into a local, so indices stay in bounds — but the design's "exactly one
painter, ever" is true of `render` and not of the buffer it paints from, and
the screen can carry two interleaved reports. The `PAINTING` latch shape
extends to `capture` if this is ever seen.
