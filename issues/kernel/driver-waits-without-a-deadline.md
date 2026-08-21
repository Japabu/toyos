---
status: open
kind: defect
opened: 2026-08-08
---

# Four driver waits spin with no deadline, and NVMe reads the timeout the spec gives it and throws it away

`grep -rn "spin_loop()" kernel/src/drivers/` returned 23 sites when this was
filed. Four of them are still unbounded polls of a device register, all on the
boot path:

- `nvme.rs:705-707` — `while bar.read_u32(REG_CSTS) & 1 != 0` (controller
  disable).
- `nvme.rs:729-731` — `while bar.read_u32(REG_CSTS) & 1 == 0` (controller
  enable).
- `virtio.rs`'s `submit_and_wait` — polls `poll_used()` forever. (Its
  *panic-path* instance is filed in `issues/panic-path/`; this is the ordinary one.)
- `virtio.rs:746` — device reset, `while common.read_u32(COMMON_DEVICE_STATUS) != 0`.

The fifth was `nvme.rs`'s `wait_completion`, which every admin and I/O command
reached through `submit_and_wait` and which spun on the completion-queue phase
bit with no deadline. **Closed 2026-08-20**: it is bounded by `nvme.rs`'s
`COMMAND` budget through `clock::settles`, and the composition above it by
`block::OPERATION` between commands. That says nothing about the two `CSTS.RDY`
polls below, which are a different wait with a different number.

**NVMe hands the driver the bound and the driver drops it.** `CAP.TO` (bits
31:24, in 500 ms units) is defined as the worst-case time for exactly the
`CSTS.RDY` transitions above and for nothing else — a command's completion is
not one, which is why the bound `wait_completion` took is a `Budget` and not a
citation of this register. `nvme.rs:699` reads the whole
`CAP` register and `:700` takes `((cap >> 32) & 0xF)` — the doorbell stride —
out of it; nothing else in the file touches `cap`. So the one number the device
publishes about how long to wait is read into a local and discarded, and a
controller that never sets `RDY` hangs the boot with nothing on the log to say
which one.

**The primitive already exists and is not shared.** The xHCI half of this closed
on its own: those waits moved behind a bounded `settles(ready)` against
`USB_TIMEOUT_NS` = 2 s (`xhci/wait/mod.rs:126-135`, `xhci/mod.rs:319`), and the
legacy handoff is bounded by `HANDOFF_TIMEOUT_NS` = 1 s (`xhci/legacy.rs:55`,
`:177-180`). It is now written twice byte-for-byte against two constants —
`xhci/wait/mod.rs:126`, `hda.rs:758` — plus `scheduler.rs:190`'s `wait_until`,
plus an IOMMU variant that `assert!`s where the others return
(`iommu/vtd/queue.rs:125-130`, `iommu/vtd/mod.rs:271-276`). A third copy lived
in `kernel/src/drivers/hda_probe.rs` (`:979`, `:990`), deleted with the HDA
probe's whole diagnostic once the driver above it answered every question that
probe was asked for.

**Standing.** The kernel-drivers type-safety audit's F10 (deadlines
and durations as bare `u64` in two different units, so `wait_writable(500)`
compiles and means "expired at boot") and F11 (the
`wait(off, until, pred) -> Result<u32, Timeout>` primitive and its blast radius)
are the design; F11's own closing line is "**Standing.** Not filed." Two
corrections to it: its count of eight unbounded MMIO polls is **four** today,
because the xHCI sites it named closed and the NVMe completion wait closed after
them; and `CAP.TO` appears
nowhere in it. **Not** the completion track — that owns the *park* deadline
(`Instant`/`Duration`/`Deadline`, "no `0 = forever`"), never a driver register
poll, and it does not touch NVMe at all.
