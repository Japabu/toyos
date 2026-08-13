---
status: open
kind: defect
opened: 2026-08-13
---

# `usb_transport_break` counts the boot stick's own transport recovery

The verdict is `log.matches("transport broke").count() > 2`
(`tests/common/usb.rs:1468-1469`), taken over the whole boot log. That string
names no device: `usb_transport_break` boots a profile that always carries two
USB mass-storage devices — the boot stick the machine actually starts from
(slot 1, unstamped, "leaving it alone") and the profile's declared disk the
injection targets (slot 2, `usb-gate: disk 1 designated`) — and the count adds
whichever of either device's transports happened to break, staged or not.

Seen on PR #41's own CI run `31684437719`, job `94397136494` ("guest (4)"),
2026-08-13, tree `7117302` (a two-file tier relegation unrelated to USB or
xHCI):

```
FAIL usb_transport_break: the transport broke 3 times off one abandoned transfer, which can undo one recovery and no more
```

## What the log actually shows

The injected disk (slot 2) did exactly what the two-break budget in
`1cb11e7` derives: the staged fault, one recovery the abandoned transfer's
late answer undid, and a clean re-issue —

```
[kernel 0.280 cpu0] usb-storage: transport broke on SCSI 0x2a: no answer in the data phase in 2000 ms
[kernel 0.280 cpu0] xHCI: slot 2 endpoint 3 is Running, recovering
[kernel 0.280 cpu0] xHCI: slot 2 endpoint 4 is Running, recovering
[kernel 0.281 cpu0] usb-storage: transport broke on SCSI 0x2a: command phase completion code 6
[kernel 0.281 cpu0] xHCI: slot 2 endpoint 3 is Stopped, recovering
[kernel 0.281 cpu0] xHCI: slot 2 endpoint 4 is Halted, recovering
[kernel 0.281 cpu0] usb-storage: SCSI 0x2a completed on attempt 3
[kernel 0.284 cpu0] usb-gate: disk done reads=ok writes=ok refusal=true wr_err=0 healthy=true
```

Two `transport broke` lines, both on SCSI `0x2a`, both on slot 2 — the shape
the assertion's own comment budgets for. The gate finished, `wr_err=0`,
`healthy=true`, every block this run cares about verified. **The disk under
test never exceeded its budget.**

The third line is 2.3 seconds later, on a different opcode and a different
device — slot 1, the boot stick, running its own SYNCHRONIZE CACHE well after
the gate had already swept and moved on:

```
[kernel 2.616 cpu0] usb-storage: transport broke on SCSI 0x35: no answer in the status phase in 2000 ms
[kernel 2.616 cpu0] xHCI: slot 1 endpoint 3 is Running, recovering
[kernel 2.895 cpu0] xHCI: slot 1 endpoint 4 is Running, recovering
[kernel 2.896 cpu0] usb-storage: SCSI 0x35 completed on attempt 2
```

That recovery is clean too — one break, one retry, done — and the boot
finishes and shuts down normally (`Boot: complete`, `Shutting down.`, no
further errors). Nothing here is the injection: `BROKE` (`"...no answer in
the data phase"`) matches exactly once, confirmed by the `staged != 1` check
two lines above the one this row is about. The host was measurably loaded for
this run — `host: fastest boot 3039 ms against the reference 1320 ms —
liveness ceilings paid at 2.30x width` — consistent with a real
`USB_TIMEOUT_NS` (2000 ms) breach on the boot stick's own status phase rather
than anything the driver did wrong: the recovery this line describes worked
exactly as designed.

`log.matches("transport broke").count()` sums both devices' lines with no
regard for which one the injection armed, so the boot stick's unrelated,
correctly-recovered break pushes the total from 2 (the injected disk's real
budget) to 3, and the `breaks > 2` guard reads that as the injected disk
failing to come back — which it did not.

## Not the retired defect

The retired row on this test (`eleven-names-red-on-ci.md`,
`Standing::Retired` in `src/redlist.rs`) was a driver ordering bug: Bulk-Only
Reset issued before the endpoints were quiesced, so a late answer to the
abandoned transfer landed on a state machine the reset had already rewound.
`1cb11e7` and `82814f3` fixed that structurally — `restart_endpoint` now
splits at the one act that reaches the bus, and the class request cannot go
first again without deleting a parameter — and replaced the old `breaks != 1`
assertion (which this row's `what` field still names) with today's
`breaks > 2` and its re-issue-derived budget. **Today's message is a
different assertion, on a different code path, describing a different
mechanism.** Nothing in this run's log resembles the retired defect: no
`Reset Endpoint failed`, `Stop Endpoint failed`, `Set TR Dequeue failed`, or
`reset recovery failed` line fired, and the injected disk's own count never
left its budget.

## The same shape as `xhci_hid_break`

`specs/issues/hardware/xhci-hid-break-counts-any-endpoint-3.md` records the
same pattern one test over: a global string match on the whole boot log
counting a transport recovery that belongs to a device the injection never
touched, because the harness always carries more USB storage than the one
disk under test (there, the boot disk's own bulk endpoints during a slow
boot; here, the boot stick's status-phase recovery during one). Two different
tests, two different strings, two different code paths in
`tests/common/usb.rs` — a new file rather than an extension of that one — but
the same class of assertion bug, worth knowing for whoever eventually scopes
either.

## ALONE-GREEN, and what CI can and cannot say about it

The harness's own re-run-alone went green:

```
PASS usb_transport_break (6s)
ALONE usb_transport_break: GREEN, and it was alone both times — nothing the harness controls differed, so it failed once and passed once. That is a rate and not a classification.
```

`Instrument::Ci` is one guest per machine, so the wide/alone difference here
is not the dev host's contention class — there is no second guest to contend
with. It is consistent instead with this run's own measured 2.30x liveness
ceiling: a boot stick status-phase round trip that took longer than 2000 ms
once, on a host that was independently measured as slow for the whole job,
and did not take longer the second time the same job ran the test by itself
a few seconds later.

## What this leaves

One sighting, no rate — `Finding::Seen` is what the redlist records. What is
owed, for whoever picks this up: scope the `breaks` count to the slot the
injection targets (the gate already logs `usb-gate: disk 1 designated`, so
the disk under test is nameable), the same fix direction `xhci_hid_break`'s
write-up already proposes for its own global count. Until then, this
assertion can red any `usb_transport_break` run in which the boot stick
independently exercises its own transport recovery in the same boot — a
recovery-count accounting that can exceed its budget once, nondeterministically,
for reasons that have nothing to do with the injected disk's own recovery.
