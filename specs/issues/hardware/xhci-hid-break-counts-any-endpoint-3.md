---
status: open
kind: defect
opened: 2026-08-10
---

# `xhci_hid_break` counts the boot disk's endpoint 3 as a HID recovery

The verdict is `log.matches("endpoint 3 is Running, recovering").count() == 2`
(`tests/common/usb.rs:1927`), taken over the whole boot log. That string names a
*dci*, and dci 3 is the first IN endpoint of every USB device — the boot USB
mass-storage disk's bulk IN as much as a HID interrupt endpoint. **One transport
recovery on the boot disk, anywhere in the boot, reds this test**, and the
failure it prints is about HID.

Seen on CI run `31405969578`, attempt 1, shard 10, tree `4905b49`:

```
FAIL xhci_hid_break: 3 endpoint(s) were found Running after the break, want 2:
["[kernel 2.639 cpu0] xHCI: slot 1 endpoint 3 is Running, recovering",
 "[kernel 2.692 cpu0] xHCI: slot 1 endpoint 4 is Running, recovering",
 "[kernel 3.708 cpu0] xHCI: slot 1 endpoint 3 is Running, recovering",
 "[kernel 4.202 cpu0] xHCI: slot 2 endpoint 3 is Running, recovering"]
```

The first two are the disk, and the log says so directly:

```
[kernel 2.639 cpu0] usb-storage: transport broke on SCSI 0x35: no answer in the status phase in 2000 ms
[kernel 2.639 cpu0] xHCI: slot 1 endpoint 3 is Running, recovering
[kernel 2.692 cpu0] xHCI: slot 1 endpoint 4 is Running, recovering
[kernel 2.692 cpu0] usb-storage: SCSI 0x35 completed on attempt 2
```

Slot 1 of the controller at `00:02.0` is `QEMU HARDDISK`, bound at 0.278 s with
`mass storage iface=0 in=0x81/1024 out=0x2/1024` — dci 3 and dci 4, which is
exactly the pair recovered. The test's own devices are not on the machine yet:
they arrive at 2.907 s on the *second* controller (`00:03.0`), where they are
slots 1 and 2 again, and their two real breaks are the lines at 3.708 and 4.202.

**The injection cannot produce the disk's two lines.** `stage_break` is a
`HidDevice` method (`kernel/src/drivers/xhci/hid.rs:190`) with one call site,
`kernel/src/drivers/xhci/mod.rs:1144`, on the HID transfer-event path. The
string is shared because the recovery is: `quiesce_endpoint`
(`kernel/src/drivers/xhci/wait/mod.rs:218`) logs it for whatever endpoint it is
handed, and a disk reaches it through `restart_bulk`
(`kernel/src/drivers/xhci/wait/msc.rs:843`).

## What made the disk break

The shard was slow — the harness printed `host: fastest boot 2851 ms against the
reference 1320 ms`, 2.16x — and the same boot shows the known blocking-I/O
shape: `LOCK CONTENTION: 50M spins at src/vfs.rs:30:18` at 1.434 s and again at
2.222 s, and `spawn: /bin/test_rs_input_events … layout=2059ms`. A log-sink
flush holds the VFS lock across a device round trip, QEMU did not answer the
status phase inside `USB_TIMEOUT_NS`, and the driver did the right thing:
recovered both bulk endpoints and completed the command on attempt 2. Nothing in
that sequence is a defect of the kernel; the defect is that the test counts it.

## Not PR #25, and not the entry that looks like it

`git diff 7af7c20 HEAD -- tests/common/usb.rs kernel/src/drivers/` is empty, so
the assertion and both recovery paths are byte-identical to `main` at the
branch's merge base. The same job's re-run said `ALONE xhci_hid_break: GREEN`
(16 s alone against 60 s wide); shard 10's second attempt never ran the suite at
all — three `docker pull debian:sid` HTTP 500s.

`specs/issues/hardware/eleven-names-red-on-ci.md` records `xhci_hid_break` as
**0 of 5** in the probe on tree `f8f73e1`. That measurement stands and is about
a different failure — the "guest stops making progress and pays its whole
ceiling" shape, which prints a timeout. It is not cover for this one, and a
`rg xhci_hid_break` hit in that file says nothing about a red that reads like
the block above.

## What a fix has to be careful about

Narrowing the match to the slot is not enough: slot ids are per controller and
the disk is slot 1 on `00:02.0` while the mouse is slot 1 on `00:03.0`, so the
two collide. The recovery line carries slot and dci and no controller. Either
the line names its controller, or the test pairs each recovery with the
`completed with code 6 (Stall Error)` line it already requires — it asserts
those separately today, so the pairing is available. **Whatever it becomes it
still has to be exactly two, still `Running`, and still fail if a recovery is
missing**; the point is to stop counting a device the injection never touched,
not to loosen the count.

## Two more occurrences, on two unrelated PR branches

Neither below touches `tests/common/usb.rs` or `kernel/src/drivers/xhci/`, so
this is the same defect the section above already explains, not a new one —
recorded because a branch-independent recurrence is itself evidence, and
because the second one changes what "`ALONE: red again`" had been read to mean.

**2026-08-10, PR #22 (`wt/toyos-endow`), run `31424496450` attempt 1, job
`93586744461` ("guest (5)"), sha `73d0761b`.** The wide run failed this shape at
20:35:41Z:

```
FAIL xhci_hid_break: 3 endpoint(s) were found Running after the break, want 2:
["[kernel 2.592 cpu1] xHCI: slot 1 endpoint 3 is Running, recovering",
 "[kernel 2.880 cpu1] xHCI: slot 1 endpoint 4 is Running, recovering",
 "[kernel 3.758 cpu0] xHCI: slot 1 endpoint 3 is Running, recovering",
 "[kernel 4.364 cpu0] xHCI: slot 2 endpoint 3 is Running, recovering"]
```

The harness's re-run-alone then failed too, at 20:38:43Z, but on a **different**
assertion — the shape `specs/issues/build/parallel-tests-red-under-other-suites.md`
already records on the dev host, here for the first time on CI:

```
FAIL xhci_hid_break: with the break staged at the fourth completion, after the
device had been delivering, input never came back: no pointer event moved by
(2560, -1920); deltas seen: [(256, 256), (256, 256)] — the boot-time tablet is
absolute, so a relative move can only have come from the mouse that was
plugged in
```

The harness printed `ALONE xhci_hid_break: red again — the defect is real.`
and its `failures:` summary quoted the *first* (wide-run) message, because that
line always carries the original failure text regardless of what the alone
re-run actually hit. Read literally, "red again" here does not mean the same
assertion fired twice — it means the test failed twice, on two different
assertions, in the same job.

**2026-08-12, PR #35 (`codex/debug-wait-census`), run `31601325987`, job
`94129283847` ("guest (5)"), sha `d522424e`.** Both the wide run (51 s) and the
alone re-run (9 s) failed on the byte-identical endpoint-count message:

```
FAIL xhci_hid_break: 3 endpoint(s) were found Running after the break, want 2:
["[kernel 2.591 cpu0] xHCI: slot 1 endpoint 3 is Running, recovering",
 "[kernel 3.175 cpu0] xHCI: slot 1 endpoint 4 is Running, recovering",
 "[kernel 4.051 cpu0] xHCI: slot 1 endpoint 3 is Running, recovering",
 "[kernel 4.660 cpu0] xHCI: slot 2 endpoint 3 is Running, recovering"]
```

This is the first recorded case of the isolated re-run reproducing this exact
assertion rather than going green (as both `main` occurrences did) or landing
on the other shape (as the Aug-10 occurrence did). A 9 s alone run is fast
enough that host contention is not a candidate explanation for this instance —
whatever stages the disk's transport break here is not timing-sensitive in the
way the earlier occurrences suggested.

## What these two add

Four branches now (`main` twice, `wt/toyos-featcollapse` via run `31405969578`
above, `wt/toyos-endow`, `codex/debug-wait-census`), none of which touch the
test or the xHCI driver, confirms the shape is a property of the test's
assertion and the boot-disk recovery path, not of any one branch's diff. The
open question this leaves for whoever picks it up: is the count-any-dci-3 test
assertion simply wrong (the fix `tests/common/usb.rs` needs), or does the boot
disk's transport recovery have a timing dependency on the HID recovery path
that a correct test would also want to catch? This write-up does not take a
side — a recovery-path defect and a wrong test expectation both explain what
was seen, and settling it is out of scope for the branches that found it.
