---
status: open
kind: defect
opened: 2026-08-02
---

# `READY_BUDGET_NS` bounds the retries, not the boot time it claims to

Filed, not fixed. The comment says "Boot time is what is being protected, and
boot time is what this measures". It measures when to stop *starting*
attempts and bounds nothing about the one already running: a device that NAKs
indefinitely costs one CBW timeout (2 s), then Bulk-Only Reset (2 s), then two
CLEAR_FEATURE(HALT)s (2 s each) — about 10 s of the boot for one device
against a 500 ms budget, times however many such devices are on the bus.
`Profile::MetalUsb` puts six on one controller. The honest statement is that
`READY_BUDGET_NS` bounds the retries and `USB_TIMEOUT_NS` times what each
costs, and the *product* is the boot-time figure. `usb-storage.md` F11.
