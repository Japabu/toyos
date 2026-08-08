---
status: open
kind: finding
opened: 2026-08-08
---

# Metal boot is 1151 ms against QEMU's 196 ms, and the recorded accounting for it is stale

**The numbers, taken out of the committed logs rather than re-measured.** Six
healthy boots in `specs/metal-logs/2026-08-07-freeze/` report `Boot: complete` at
1148, 1148, 1149, 1150, 1151 and 1154 ms; the seventh (`…-222741.log`) is 755 ms
and is the control boot whose keyboard was refused, so its peripherals phase is
448 ms instead of 842. The QEMU figure for the comparable shape is
`Boot: complete (196ms)` on the `metal_sim_compositor` boot (`specs/issues/hardware/`'s i8042 entry
below records the measurement), and `(234ms)` for the diag artifact booted
headless. **So metal is ~5.9× QEMU, not the ~17× `specs/metal-hardware-inventory.md:392-395`
computes** — that ratio is against `(3422ms)`, and 2.30 s of those 3.42 s were
the six `boot_checkpoint` framebuffer repaints (`metal-hardware-inventory.md:425-429`),
which #138's write-combining change removed. Measuring the phase-boundary gaps in
`…-223244.log` myself, all six together are **73 ms** against that boot's 2308.
The inventory's "Boot timing on metal" section describes a machine that no longer
exists and should be re-taken or dated.

**Where the 1151 ms goes now**, from `…-223244.log`:

| phase | reported |
|---|---|
| CPU ready | 60 ms |
| storage ready | 84 ms |
| **peripherals ready** | **842 ms** |
| subsystems ready | 93 ms |
| devices ready | 20 ms |

Peripherals is 73% of the boot, and its two largest components are:

- **393 ms of i8042 keyboard init** — `i8042: ok selftest=0x55` at 0.609, the
  next i8042 line at 1.002. Real hardware, not a probe of an absent one.
- **206 ms establishing the Thunderbolt xHC at `00:0d.0` has nothing on any
  port** — `controller started` at 0.161, `no HID devices on the controller at
  00:0d.0` at 0.367. Four of the PCH's port resets are 55 ms each, which is USB's
  own and not the driver's to shorten.

**Absent-device probing is ~279 ms of 1151, not ~1.1 s.** The other piece is the
PCI walk: `PCI: Enumerating devices...` at 0.065, last real function `0a:00.0` at
0.072, `Enumeration complete, 24 functions.` at 0.145 — **73 ms** scanning buses
that hold nothing, against 7 ms finding everything that is there.

**What this entry is for.** Metal boot time has no heading of its own; the
accounting lives in `specs/metal-hardware-inventory.md` against the superseded
3422 ms boot, and `specs/issues/`'s console entry below points at "#65 (boot
time)" as its owner. Whatever #65 says, its numbers should come from this table:
the two-thirds that motivated it were paints and are gone. Note also the NIC
retry that looks like boot cost and is not — `toyos/src/net.rs:271`'s 100 retries
at 10 ms run *after* `Boot: complete` (see *Every network client pays a second of
boot retry on a machine with no NIC*), and `READY_BUDGET_NS` bounds retries
rather than boot time (`specs/issues/filesystem/`).
