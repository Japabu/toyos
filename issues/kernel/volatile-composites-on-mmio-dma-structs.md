---
status: open
kind: defect
opened: 2026-08-20
---

# `write_volatile`/`read_volatile` over a whole DMA/MMIO struct, ten times

`clippy::volatile_composites` (nursery) measured **10**, kernel-only, all in
driver code that hands a composite type straight to `write_volatile` or
`read_volatile` instead of touching each primitive field on its own:

- `drivers/nvme.rs:101,119` — `SqEntry`/`CqEntry`, the submission/completion
  queue entries.
- `drivers/xhci/wait/boot.rs:446` — `ErstEntry`.
- `drivers/xhci/mod.rs:560,593,1093` — `Trb`, three sites.
- `drivers/virtio.rs:414,487` — the virtio descriptor.
- `drivers/virtio_gpu.rs:276` — `RespEdid`.

The lint's own docs put the reason plainly: volatile ops are well-defined for
a primitive because the compiler must emit exactly the load/store the source
says, in order — the property MMIO needs. That guarantee is **not** extended
to composite types; how rustc lowers a whole-struct `write_volatile` (one
store, several, in what order) is implementation-defined and can change
between compiler versions. The fix the lint wants is per-field volatile
access via pointer arithmetic.

**Not adopted this pass.** This is not mechanical: at least the xHCI `Trb`
sites carry a real protocol constraint the lint can't see — hardware must not
observe the TRB as valid until its Control field (Cycle bit) is written, so
splitting a `write_volatile(ptr, trb)` into per-field stores has to preserve
that ordering by hand, on a path this driver depends on for every command and
transfer. Getting that wrong silently (works in QEMU, wrong on some real
controller's stricter timing, or vice versa) is worse than the current
implementation-defined-but-tested state. NVMe's `SqEntry`/`CqEntry` and
virtio's descriptor likely have their own per-field ordering rules from their
respective specs, not derived here.

Fixing this is real driver work: read each device's spec for what field
ordering its rings actually require, decide per site whether one
`write_volatile` is provably fine (e.g. the struct is small enough that the
target's ISA guarantees a single-instruction store, which is again not
something the *language* promises) or needs splitting, and add the
regression coverage that would have caught the difference. Filed rather than
fixed — `issues/build/clippy-has-never-run-here.md` records the measurement
and the reason it wasn't attempted here.
