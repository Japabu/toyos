---
status: open
kind: defect
opened: 2026-08-21
---

# `fsync` on `/log` returned `Kind(Other)` with twelve guests on the host

`esp_filesystem`'s guest binary writes a 41,097-byte blob to `/log`, calls
`sync_all`, and reads it back. On 2026-08-21, in a full `cargo test` on the dev
host (branch `wt/toyos-returnrule` at `02a087fd`, 79 guests in the run, host
note `fastest boot 2058 ms against the reference 1320 ms — liveness ceilings
paid at 1.56x width`), the `sync_all` failed:

    thread 'main' (1) panicked at src/bin/esp_files.rs:130:22:
    fsync the blob: Kind(Other)

    FAIL  esp_filesystem  (12s)

The five checks before it all passed — the host note read back through `/boot`,
both directory listings, `BOOTx64.EFI`'s 513,536 bytes, and every refusal on the
read-only `/boot` mount. Only the `/log` write path failed, and only at the
flush.

**The harness's own re-run says `ALONE esp_filesystem: GREEN` (4 s), which is a
hypothesis and not a finding** — `tests/CLAUDE.md` is explicit that a red whose
mechanism could be a kernel race must not be answered by re-classifying its
`Sched`. `cargo run -- --known-red esp_filesystem` answers `NOT ON THE LIST`, so
nothing adjudicates it and every author who meets it re-derives this.

**What makes it worth a file rather than a shrug**: `SYS_FSYNC` reaching the
device's cache flush is what `/bin/logd`'s durability claim rests on, and
`issues/audio/disk-wait-pins-a-cpu.md` measures that every userland file
write-back sits four ticket spinlocks deep with preemption off for the whole
device round trip. An `fsync` that *returns an error* under contention is a
different failure from that one — pinning is slow, not wrong — and this is the
first sighting of the error return. If the flush can fail because the host is
busy, then either the error is spurious and `logd` ends a boot's log for nothing,
or it is real and a durability claim has a load-dependent hole. `Kind(Other)`
does not say which.

The measurement to take: the path from `sync_all` through `object/ops.rs`'s
`fsync` to the FAT32 adapter's flush, asking which layer manufactures the error
and whether any deadline in it is a wall-clock one — a deadline sized on an
unloaded host is exactly what a 1.56x-width run breaks. `usb-slow-device` (kernel
feature) already stages a slowed device round trip and is where the negative
control lives.

Not caused by the change it was seen on: `02a087fd` moves three tier
declarations and one validator constant, and touches no kernel, driver,
filesystem or SDK code at all. It did change the fast partition's composition
(272 tests rather than 275), which changes what runs beside what.
