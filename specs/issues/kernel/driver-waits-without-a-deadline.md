---
status: open
kind: defect
opened: 2026-08-08
---

# Five driver waits spin with no deadline, and NVMe reads the timeout the spec gives it and throws it away

`grep -rn "spin_loop()" kernel/src/drivers/` returns 23 sites. Five of them are
unbounded polls of a device register, all on the boot path:

- `nvme.rs:105-119` `wait_completion` — `loop { … core::hint::spin_loop(); }`
  on the completion-queue phase bit, no deadline. **Every** admin and I/O
  command reaches it through `submit_and_wait` (`:121-124`).
- `nvme.rs:434-436` — `while bar.read_u32(REG_CSTS) & 1 != 0` (controller
  disable).
- `nvme.rs:458-460` — `while bar.read_u32(REG_CSTS) & 1 == 0` (controller
  enable).
- `virtio.rs:412-417` `submit_and_wait` — polls `poll_used()` forever. (Its
  *panic-path* instance is filed in `specs/issues/panic-path/`; this is the ordinary one.)
- `virtio.rs:453-456` — device reset, `while common.read_u32(COMMON_DEVICE_STATUS) != 0`.

**NVMe hands the driver the bound and the driver drops it.** `CAP.TO` (bits
31:24, in 500 ms units) is defined as the worst-case time for exactly the
`CSTS.RDY` transitions at `:434` and `:458`. `nvme.rs:429` reads the whole
`CAP` register and `:430` takes `((cap >> 32) & 0xF)` — the doorbell stride —
out of it; nothing else in the file touches `cap`. So the one number the device
publishes about how long to wait is read into a local and discarded, and a
controller that never sets `RDY` hangs the boot with nothing on the log to say
which one.

**The primitive already exists and is not shared.** The xHCI half of this closed
on its own: those waits moved behind a bounded `settles(ready)` against
`USB_TIMEOUT_NS` = 2 s (`xhci/wait/mod.rs:126-135`, `xhci/mod.rs:319`), and the
legacy handoff is bounded by `HANDOFF_TIMEOUT_NS` = 1 s (`xhci/legacy.rs:55`,
`:177-180`). It is now written three times byte-for-byte against three
constants — `xhci/wait/mod.rs:126`, `hda.rs:758`, `hda_probe.rs:979` — plus two
copies of the spin delay beside it (`hda.rs:769`, `hda_probe.rs:990`), plus
`scheduler.rs:190`'s `wait_until`, plus an IOMMU variant that `assert!`s where
the others return (`iommu/vtd/queue.rs:125-130`, `iommu/vtd/mod.rs:271-276`).

**Standing.** `specs/type-safety-audit/kernel-drivers.md` F10 (`:928`, deadlines
and durations as bare `u64` in two different units, so `wait_writable(500)`
compiles and means "expired at boot") and F11 (`:987`, the
`wait(off, until, pred) -> Result<u32, Timeout>` primitive and its blast radius)
are the design; F11's own closing line is "**Standing.** Not filed." Two
corrections to it: its count of eight unbounded MMIO polls is **five** today,
because the xHCI sites it named are the ones that closed; and `CAP.TO` appears
nowhere in it. **Not** `specs/iouring-blocking-spec.md` — that spec owns the
*park* deadline (§9.1–9.2, `Instant`/`Duration`/`Deadline`, "no `0 = forever`"),
never a driver register poll, and does not mention NVMe at all.
