---
status: open
kind: finding
opened: 2026-08-01
---

# No machine in reach can make the xHCI legacy handoff fail, so its gate certifies only that the walk runs

Closed at `755b591` + `d83c53b`: `kernel/src/drivers/xhci/legacy.rs` walks the
extended-capability list from `HCCPARAMS1.xECP`, and when it finds capability
ID 1 it sets the OS-Owned Semaphore, waits a bounded second for the BIOS-Owned
Semaphore to clear, and clears USBLEGCTLSTS's SMI enables and its RW1C status
bits whatever the semaphore said. It runs immediately before the halt and
HCRST, an absent capability and a malformed list both cost the handoff and
never the boot, and the driver proceeds either way — a machine that will not
boot is worse than one whose firmware is fighting it, and the point is the log
line naming the fight.

**What remains is that no machine in reach can fail it.** QEMU's controller
publishes an extended-capability list with no Legacy Support capability in it
(`xECP=0x8`, measured), and nothing owns the controller once OVMF's USB stack
releases it at ExitBootServices. So a green `xhci_xecp_walk` certifies exactly
two things: the walk runs on a real controller and terminates, and it runs
*before* HCRST rather than after. Both halves of the interesting behaviour —
firmware that holds the semaphore, and firmware with SMI-on-OS-ownership armed
— are first observed on the T14 or not at all.

The untrusted-input half is testable and is tested, because it needed no
hardware: `xhci-xecp-selftest` walks eight synthetic lists at init (a pointer
past the register window, a link that leaves it, a window reading all ones, a
chain of minimum-length links, ours first/last/absent) and logs how many were
refused. The walk cannot loop, for three independent reasons — the next pointer
is a strictly positive forward delta, every read is bounds-checked against the

also the leading suspect for the T14's five ports that reset and did not enable,
and it was not the cause — see `2b0631f`; the reset write is the same for both
protocols, and knowing which port is which would not have changed it. What it
*is* needed for is `port-reset-gets-no-second-try`.
