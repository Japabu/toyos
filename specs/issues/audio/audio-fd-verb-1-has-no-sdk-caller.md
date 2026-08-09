---
status: open
kind: finding
opened: 2026-08-01
---

# The kernel's byte-1 audio fd verb has no SDK caller

`kernel/src/fd.rs` still dispatches `1 => crate::audio::start()`, but
suspend-on-idle deleted `AudioDev::start()` from `toyos/src/device.rs`: the only
PCM start left is the implicit one inside `submit_buffer`, which is what makes
resume a single control verb inline with the first submit.

Recorded rather than deleted, deliberately — a dead-code sweep that removes the
arm narrows the ABI, and the syscall surface is a contract, not an implementation
detail. Byte 0 (stop) is live; soundd calls it every suspend.
