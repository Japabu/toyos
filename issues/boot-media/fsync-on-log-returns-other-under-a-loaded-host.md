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

Not caused by the change it was seen on: `02a087fd` moves three tier
declarations and one validator constant, and touches no kernel, driver,
filesystem or SDK code at all. It did change the fast partition's composition
(272 tests rather than 275), which changes what runs beside what.

## The device under `/log` in this test, established by reading

`esp_filesystem` boots `Profile::Metal` (`tests/common/volumes.rs:286-295`),
whose `Shape` sets `storage_bus: "xhci.0"` (`tests/common/qemu.rs:1381`), and the
boot image is attached as `usb-storage,bus=xhci.0` (`:3062-3066`). So `/boot` and
`/log` are **USB mass storage over xHCI**, not NVMe and not virtio: the flush is
SCSI SYNCHRONIZE CACHE (10), opcode `0x35`, issued at
`kernel/src/drivers/xhci/wait/msc.rs:551`. `virtio` is `Absent` in this shape and
the NVMe namespace is a separate scratch disk.

## What `Kind(Other)` can mean here

`rust/library/std/src/sys/fs/toyos.rs:13-24` names six `SyscallError` variants
and sends *everything else* to `io::ErrorKind::Other`. Of the five it does not
name — `Unknown`, `BadAddress`, `NotSupported`, `Io`, `Gone` — only
`SyscallError::Io` is reachable on the fsync path. So the sighting's
`Kind(Other)` is `SyscallError::Io` and nothing else: a **device or volume I/O
refusal**, never a refused input (`InvalidArgument`/`PermissionDenied`/`NotFound`
all have their own kinds) and never resource exhaustion (`ResourceExhausted` maps
to `OutOfMemory`).

## Every error site on the path, and what kind each is

`(a)` wall-clock or tick-count deadline · `(b)` device status · `(c)` refused
input · `(d)` resource exhaustion. Read-only frames marked *sysroot*.

| site | what refuses | kind |
|---|---|---|
| `rust/library/std/src/sys/fs/toyos.rs:297,236-238` | `sync_all` → `File::fsync` → `syscall::fsync`, mapped by `to_io_error` (*sysroot*) | — |
| `rust/library/std/src/sys/fs/toyos.rs:13-24` | flattens `Io` (and four others) to `Kind(Other)` (*sysroot*) | — |
| `kernel/src/arch/syscall.rs:337` | handle is not one, or lacks `Rights::WRITE` | (c) |
| `kernel/src/object/ops.rs:606-608` | the object is not a `File` → `PermissionDenied` | (c) |
| `kernel/src/object/ops.rs:619-621` | `vfs.flush_file` refused | pass-through |
| `kernel/src/object/ops.rs:627-629` | `vfs.sync_for_path` refused | pass-through |
| `kernel/src/vfs.rs:523,525` | empty mount or empty fs path → `InvalidArgument` | (c) |
| `kernel/src/vfs.rs:524` | `resolve_fs` found nothing → `NotFound` | (c) |
| `kernel/src/vfs.rs:545` | `fs.write_page` refused (one call per dirty page; the blob is eleven) | pass-through |
| `kernel/src/vfs.rs:551` | `fs.update_metadata` refused | pass-through |
| `kernel/src/vfs.rs:683` | `sync_mount` on a name that is not mounted → `NotFound` | (c) |
| `kernel/src/vfs.rs:713-726` | `sync_for_path` → `sync_mount`, or the root's `sync` | pass-through |
| `kernel/src/fat32_adapter.rs:751,777` | no open-file record for the `FileId` → `NotFound` (kernel invariant) | (c) |
| `kernel/src/fat32_adapter.rs:752-755` | `Fat32::write` refused | pass-through |
| `kernel/src/fat32_adapter.rs:779-783` | `set_len` / `flush_meta` refused | pass-through |
| `kernel/src/fat32_adapter.rs:810-812` | `Fat32::sync` refused — deliberately unlogged, so this one leaves no line | pass-through |
| `kernel/src/fat32_adapter.rs:549-569` | `as_syscall_error`: `Io`/`NotFat32`/`Truncated`/`CorruptChain`/`CorruptDirectory` → `Io`; `NoSpace`/`TooLarge`/`LimitExceeded` → `ResourceExhausted`; the rest → `InvalidArgument` | (b)/(d)/(c) |
| `toyos-fat32/src/fs.rs:901-910` | `Fat32::sync`: the FSInfo write, then `dev.flush()` | pass-through |
| `toyos-fat32/src/fs.rs:193,468,524` | a `scratch`/slice bound → `Error::Io` — a kernel invariant reported as a device fact | (c) *misreported as (b)* |
| `toyos-fat32/src/error.rs:46-49` | `From<IoError> for Error` → `Error::Io`: every device refusal becomes one word here | (b) |
| `kernel/src/fat32_adapter.rs:203-208` | `locate`: the request leaves the partition → `IoError` | (c) |
| `kernel/src/fat32_adapter.rs:337-338,353-354,357-359` | the volume slot is empty → `IoError` | (c) |
| `kernel/src/fat32_adapter.rs:339-348` | `fat-backing-read-fails` actuator (reads only) | injected |
| `kernel/src/fat32_adapter.rs:223,263,289,299,359` | `read_blocks`/`write_blocks`/`flush` refused | pass-through |
| **`kernel/src/block.rs:42-46,61-63`** | **`OPERATION` — 2 s, `Deadline::at(clock::now() + 2 s)`** | **(a)** |
| `kernel/src/drivers/usb_storage.rs:104-129` | the three trait methods; each opens a fresh 2 s budget **before** `xhci::storage_*` takes the `XHCI` ticket lock | (a) composition |
| `kernel/src/drivers/xhci/mod.rs:2054-2066` | `with_disk`: `XHCI.lock()`, then no disk under that index → `None` → `false` | (b) |
| `kernel/src/drivers/xhci/wait/msc.rs:546-548` | `dev.failed` — a disk recovery already gave up on | (b) |
| `kernel/src/drivers/xhci/wait/msc.rs:553-556,571-578` | `flush_sense()` actuators; `Scsi::Refused` → `log_refusal` → `false` | (b) |
| `kernel/src/drivers/xhci/wait/msc.rs:607-613` | `lba + count` past the disk's block count | (c) |
| `kernel/src/drivers/xhci/wait/msc.rs:646-660` | short transfer, `Scsi::Refused`, `Scsi::Broken` | (b) |
| **`kernel/src/drivers/xhci/wait/msc.rs:723-728`** | **`until.reached(clock::now())` → `Scsi::Broken`, `block::OPERATION`'s 2 s** | **(a)** |
| `kernel/src/drivers/xhci/wait/msc.rs:741-747` | `reset_recovery` failed → `dev.failed = true` | (b) |
| `kernel/src/drivers/xhci/wait/msc.rs:751-753` | `MAX_TRANSPORT_ATTEMPTS` (3) exhausted | (b), each attempt's break may be (a) |
| `kernel/src/drivers/xhci/wait/msc.rs:799-807` | `framed_phase`: `Short`, `Code`, `Silence` | (b), `Silence` is (a) |
| `kernel/src/drivers/xhci/wait/msc.rs:919-938` | CSW signature, tag, residue, phase error | (b) |
| **`kernel/src/drivers/xhci/wait/mod.rs:363-375`** | **`wait_transfer`: `USB_TIMEOUT_NS`, 2 s (`xhci/mod.rs:376,383`)** | **(a)** |
| `kernel/src/drivers/xhci/wait/mod.rs:384-386` | the port reads disconnected → `None` | (b) |

Two `(a)` deadlines, both 2 s, and **both are host wall clock**:
`kernel/src/clock.rs:103-117`'s `nanos_since_boot` is the TSC, and a TCG guest's
TSC advances with the host's real time rather than with the guest's work. Nothing
on this path is bounded by operation count or by guest ticks.

## Where the deadlines are paid, and what a loaded host does to them

`kernel/src/drivers/usb_storage.rs:123` opens the 2 s budget and *then*
`xhci::storage_flush` takes the `XHCI` ticket lock
(`kernel/src/drivers/xhci/mod.rs:2055`), so `XHCI` lock-wait and any host
descheduling of the vCPU thread are charged to the *device's* budget. This is the
composition `issues/audio/disk-wait-pins-a-cpu.md` measures from the other side:
four ticket spinlocks deep, preemption off, for the whole round trip.

**A live device has already been recorded reaching one of these bounds.**
`issues/hardware/usb-transport-break-counts-the-boot-sticks-recovery.md` carries
the log, on a host measured at 2.30x width, of the boot stick's *own* flush:

    [kernel 2.616 cpu0] usb-storage: transport broke on SCSI 0x35: no answer in the status phase in 2000 ms
    [kernel 2.896 cpu0] usb-storage: SCSI 0x35 completed on attempt 2

Same opcode, same device class, same partition — `USB_TIMEOUT_NS` breached on the
status phase of SYNCHRONIZE CACHE by a stick that answered the identical command
280 ms later. That falsifies the constant's own claim that "nothing but a dead
device can reach it".

**And it dates the defect.** That break was *absorbed*: `SCSI 0x35 completed on
attempt 2`, the write not lost, the boot fine. It could be, because
`block::OPERATION` did not exist yet — `5479129d`, "A block-device operation
carries the caller's budget, and the driver honours it", landed **2026-08-20**,
seven days after that log and one day before this issue's sighting. Since it
landed, the recovery that used to save this write is refused before it can be
re-issued, because the budget above the driver is exactly as long as the transfer
bound below it. The retry machinery `MAX_TRANSPORT_ATTEMPTS` = 3
(`kernel/src/drivers/xhci/wait/msc.rs:87`) provides has been unreachable-on-timeout
ever since.

## What a spurious refusal costs

*(As of 2026-08-22 this section describes what the tree used to do; "what is
owed" item 3 below carries what replaced it.)*

`/bin/logd` treated any `Err` from `sync_all` as final: `volume = None` and the
boot's log console-only from that point, for the rest of the boot.
`LOG_WRITE_BUDGET` (5 s) was explicitly *not* the policy for errors — its own
doc said "an **error** ends it at once" — and it bounded only the case where the
calls succeed slowly. So logd was written to tell "busy" from "gone" and the
kernel gave it one word for both.

## Reproduced, 2026-08-22, and the producer is both deadlines in series

73 consecutive full 12-wide `cargo test --test toyos-build` suites on the dev
host, `wt/toyos-fsync` at `8c0f9526`, 12:09:11Z to 13:09:27Z, 272 tests each.
`esp_filesystem` **red once in 73**; the harness's own re-run of that red was
`ALONE esp_filesystem: GREEN`, as in the sighting. `wt/toyos-dmapool` had its own
suite on the same twelve guest slots for 21 of the 73 passes, the red among them.
Host load average 2.00 at the start and 11.86 at the end.

The red's kernel log — which only exists because this branch stopped the failure
arm dropping it:

    [kernel 2.606 cpu0] usb-storage: 00:02.0 slot 1 transport broke on SCSI 0x2a: no answer in the status phase in 2000 ms
    [kernel 2.606 cpu0] xHCI: 00:02.0 slot 1 endpoint 3 is Running, recovering
    [kernel 2.607 cpu0] xHCI: 00:02.0 slot 1 endpoint 4 is Running, recovering
    [kernel 2.607 cpu0] usb-storage: 00:02.0 slot 1 SCSI 0x2a not issued: 2000ms (the block-device operation is refused with an I/O error, and the caller's own give-up policy decides what happens next)
    [kernel 2.607 cpu0] usb-storage: write of 1 blocks at 87364 failed on disk 0
    [kernel 2.609 cpu0] log-volume: write of guest-blob.bin: device I/O failed

    thread 'main' (1) panicked at src/bin/esp_files.rs:130:22:
    fsync the blob: Kind(Other)
    [kernel 2.671 cpu0] syscalls: pid=7 total=546 syscall_wall=2108ms ...
    [kernel 2.671 cpu0] exit: test_rs_esp_files pid=7 code=101 cpu=2139ms

**Both `(a)` deadlines fired, and the second one is a consequence of the first.**

1. `wait_transfer`'s `USB_TIMEOUT_NS` was breached on the **status phase of a
   WRITE(10)** during `flush_file`'s page write-back — not on the flush, and not
   on a dead device: both endpoints read *Running*, so the transfers were still
   the controller's to complete.
2. Reset Recovery ran and succeeded, in 1 ms
   (`kernel/src/drivers/xhci/wait/msc.rs:1018`).
3. Attempt 2 was then refused at `msc.rs:723-728` without being issued, because
   `block::OPERATION` is **2 s and `USB_TIMEOUT_NS` is also 2 s**: one breached
   transfer spends the entire operation budget, so `MAX_TRANSPORT_ATTEMPTS` = 3
   is unreachable whenever the break was a timeout.
4. `false` → `BlockError` → `IoError` → `Error::Io` → `SyscallError::Io` →
   `Kind(Other)`.

That is exactly the failure the retry loop's own doc says it exists to prevent
(`msc.rs:674-680`): "a driver that recovers and then reports failure has thrown
away a write it could have completed — the T14's boot disk losing a block to one
transport hiccup". The driver recovered and then reported failure, and the budget
above it made that inevitable rather than possible.

**The aggregate width does not predict it.** The failing pass measured
`fastest boot 1385 ms … 1.05x width`, the loop's median; the sighting's was
1.56x. `fastest boot` is a minimum over 78 guests, so it says nothing about the
one guest that lost its vCPU — and that guest spent `syscall_wall=2108ms`, all of
it inside one `SYS_FSYNC`, in a boot whose peers were up in 1,385 ms.

## What was measured, 2026-08-22, dev host

- `usb-slow-device` (2 ms per mass-storage bulk completion,
  `kernel/src/drivers/xhci/mod.rs:1208-1242`) **does not stage this**. Armed on
  the `esp_filesystem` path it is `PASS esp_filesystem (3s)` at 1.53x width. The
  arithmetic says why: `msc_flush` is one SCSI command, three bulk transfers,
  6 ms — three orders of magnitude under either 2 s bound. The negative control
  the first draft of this file named is the wrong instrument for it.
- `usb-flush-fails` (SCSI sense `0x04/0x44/0x00` on `0x35`) **does** stage the
  userland symptom exactly: `fsync the blob: Kind(Other)` at
  `src/bin/esp_files.rs:130:22`, `ALONE esp_filesystem: red again`. So the
  observed panic is one `SyscallError::Io` from the flush and nothing narrower.
- The deadline branch is the *same* value by construction, not by experiment:
  `msc.rs:727` returns `Scsi::Broken` and `msc.rs:571-578` maps `Scsi::Broken`
  and a transport failure to the identical `false`. A budget refusal — which
  `kernel/src/block.rs:38-41` says is "a degraded answer" that does not mark the
  device failed — and a stick that cannot flush are indistinguishable from
  `FatFs::sync` upwards.
- Baseline `esp_filesystem` alone: green at 1.56x, 1.53x, 1.49x and 1.48x width,
  i.e. at the sighting's own width. `toyos-fat32-check` is silent on the volume
  before and after every one of those boots.

## What is owed

1. **`OPERATION` and `USB_TIMEOUT_NS` are both 2 s, and in series that makes the
   transport's retry dead on any timeout-induced break.** `block.rs`'s derivation
   reads "Below: one whole `USB_TIMEOUT_NS`, so a caller that has spent more than
   a single transfer's entire allowance on commands that are *completing* is
   talking to a device too slow to serve" — the word doing the work is
   *completing*, and a transfer that breached its own bound did not complete. For
   `MAX_TRANSPORT_ATTEMPTS` to mean anything the operation has to outlast one
   breached transfer plus one Reset Recovery plus one re-issue. **Not an agent's
   number to move**: `OPERATION` is derived against `/bin/logd`'s
   `LOG_WRITE_BUDGET` of 5 s on one side and against the CPU pin
   `issues/audio/disk-wait-pins-a-cpu.md` measures on the other, so raising it
   lengthens an audio-path stall. The alternatives — a shorter `USB_TIMEOUT_NS`,
   or `MAX_TRANSPORT_ATTEMPTS` cut to 1 with the doc saying that a timed-out
   transfer is never retried — are the same trade seen from the other end.
   Owner's call.
2. **The two `(a)` bounds are host wall clock and were documented as unreachable
   by a live device** (corrected at both sites on 2026-08-22). The tree has
   nothing to convert them *to* — every other bounded wait in it is
   `clock::settles` on the same TSC — and for real hardware a time bound is the
   correct bound, so this is not a "replace the clock" fix.
3. ~~**`BlockError` is one bit and a budget refusal is not a device fact.**~~
   **Done, 2026-08-22.** `BlockError` is `Device | BudgetExpired`, minted apart
   by `XhciController::scsi` (`Scsi::Budget`) and by NVMe's `may_issue`, carried
   through `toyos_fat32::IoError`/`Error`'s matching pair, and answered by
   `as_syscall_error` as `SyscallError::WouldBlock` — the word the ABI already
   had. `/bin/logd`'s `policy::fate` keeps the volume across a flush that would
   block and still ends it on a device fact, bounding the *run* of retries by
   `LOG_WRITE_BUDGET` so a permanently loaded host does not keep a volume nobody
   is writing to. What is **not** covered is an end-to-end guest arm: staging a
   budget-expired flush needs one actuator and
   `issues/build/the-actuator-arm-set-is-full-at-64.md` is why there is no 65th.
   The mint is gated by `cache_eviction`'s
   `nvme-gate: read with a spent budget refused=true budget=true` and
   `usb_storage_gate`'s `usb-gate:` line of the same shape, the mapping by
   `toyos-fat32`'s `a_budget_that_expired_is_not_a_device_that_failed` and
   `a_flush_says_which_of_the_two_refusals_it_was`, and the decision by
   `logd::policy`'s six.
4. `Broke::Silence`'s `Display` (`kernel/src/drivers/xhci/wait/msc.rs:233-237`)
   says "no answer in the {phase} phase in 2000 ms" on both exits from
   `wait_transfer` — including `wait/mod.rs:384-386`, where the port read
   disconnected and no 2000 ms elapsed. A pulled stick is logged as a timeout.
