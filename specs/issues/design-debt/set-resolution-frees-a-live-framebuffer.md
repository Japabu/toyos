---
status: open
kind: defect
opened: 2026-07-31
---

# `gpu::set_resolution` frees the old framebuffer while consumers may hold pointers to it

`kernel/src/gpu.rs:59-76` calls into the driver, and virtio's implementation
allocates a new framebuffer and frees the old one. Today the only consumer
re-reads `GpuInfo` afterwards, so nothing breaks; the pattern is simply
unguarded for anything that caches the address. The panic console is the first
thing that would have cached one, and it handles the window explicitly —
`detach()` before the call, `rearm()` if the driver refused — which is a
per-caller workaround, not a fix. The fix is for `set_resolution` to own the
invalidation.
