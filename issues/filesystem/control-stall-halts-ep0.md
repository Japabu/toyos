---
status: open
kind: defect
opened: 2026-08-02
---

# A control transfer that stalls during enumeration leaves EP0 halted for good

Filed, not fixed, and visible on any boot of `Profile::MetalFullSpeed`:
QEMU's `usb-wacom-tablet` stalls SET_PROTOCOL, and the driver logs
`xHCI: SET_PROTOCOL on port 6: status stage completion code 6 (Stall Error)` and
carries on.
A stall halts EP0, and nothing clears it — there is no `restart_endpoint` for a
control endpoint. Harmless today because enumeration issues no further control
transfer to that device and the interrupt endpoint is configured afterwards
regardless, so the tablet binds and delivers. It stops being harmless the moment
anything wants to talk to a bound HID over EP0, which is what the mass-storage
path already does on its recovery path.

The same hole one level up: if `reset_recovery`'s Bulk-Only Reset request itself
stalls, EP0 is halted and only the *bulk* endpoints are restarted.
