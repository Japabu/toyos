---
status: open
kind: finding
opened: 2026-08-01
---

# `f32::round()` lowers to a `compiler_builtins` call, not `roundss`

On the ToyOS target the quantizer's `round()` is a `roundf` libcall, once per
sample — 256 per period at roughly 344 periods/s, so about 88k calls a second.

SSE4.1 is universally present on the 2020+ hardware baseline, so enabling it in
the target spec turns this into one instruction. Whether to widen the target's
feature set is a separate decision and a larger one than this call site.
