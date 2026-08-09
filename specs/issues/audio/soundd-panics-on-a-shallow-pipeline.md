---
status: open
kind: defect
opened: 2026-08-01
---

# A device advertising four buffers panics soundd at startup

`assert!(num_buffers > 5, "soundd: pipeline too shallow to defer safely")`
(`soundd/src/main.rs:597`) turns a device shape into a startup panic. Same class
as the NVMe and xHCI zero-device panics that closed on 2026-08-01 — an
unanticipated device shape killing a process rather than being handled — and
**metal-relevant, since nobody knows what the T14's codec advertises.**

The fix falls out of decoupling the client slot count from the device pipeline
depth, which turns the assert into a clamp. Which is also *why* the assert
exists: `slot_count = num_buffers` (`main.rs:1290`) couples every client's ring
geometry to the kernel's `TX_INFLIGHT_MAX`. The comment's own reasoning
establishes `slot_count >= num_buffers`; **equality was assumed, not derived.**

That design is written up and deliberately not landed — it changes ring geometry
and therefore audio timing, so it needs gate A's thorough tier on a quiet tree,
not the fast one.
