---
status: open
kind: finding
opened: 2026-08-02
---

# The xHCI driver refuses a controller whose PAGESIZE does not include 4 KiB

`init` logs OP_PAGESIZE and refuses the controller by PCI address if bit 0 —
the bit that says 4 KiB — is clear. Every structure the driver places — rings,
contexts, scratchpad buffers — is sized and aligned to a hardcoded 4 KiB, so a
controller that cannot do 4 KiB is unimplemented, not merely unusual, and the
machine says so at init instead of corrupting memory silently.

**It used to `assert_eq!(pagesize, 1)`, which was wrong twice** and is fixed at
`5fde1c5`. The register is a *mask* of the page sizes the controller supports,
so equality is stricter than the requirement (Linux reads it with `ffs()`);
and a panic takes the machine for one controller's property on a laptop that
has two, which is the exact failure the drive-every-controller work exists to
prevent. Sixty lines above, a controller with neither MSI-X nor MSI is refused
by name and `init` carries on. Both are equally fatal to that controller and
neither is fatal to the machine; now both read the same.

The scratchpad is the whole exposure. Its entries are one 4 KiB page apart,
so at PAGESIZE 8 KiB with `max_scratchpad = 8` entry 7 sits at 0xF000 and the
controller writes [0xF000, 0x11000) — over entry 6 and into block 0's
interrupt ring at `dev_base`. Every other consequence runs the safe way: a
larger page size only relaxes the rule that the DCBAA and the device contexts
must not cross one.

What is still not built is honouring such a controller. If a machine ever
trips the assert, the fix is to derive `PAGE` from the register instead of
raising the bound.
