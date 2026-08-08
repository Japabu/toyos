---
status: open
kind: defect
opened: 2026-08-08
---

# `xhci_flap`'s fourth collapsed replug wedges the driver under KVM

Run `31246245541`, `debian:sid`/QEMU 11.0.3/KVM, `--jobs 1`, alone:
`timed out after 164s`. Green under TCG on the same runner image and the same
QEMU, and green on the dev host.

Three of the four cycles are in the log and all three are the state under test —
`port 5 was unplugged and plugged back in between two looks; tearing the old
device down before enumerating what is there now`, followed each time by a clean
re-enumeration and `pointer on slot 1 merges as source 2`, at 1.879, 3.084 and
3.787 s. Then nothing: the fourth `device_del`/`device_add` pair goes in at about
4.4 s and the guest never speaks again, never delivers the pointer motion the
test ends on, and `test_rs_input_events` therefore never exits.

So the driver survives three collapsed replugs and stops on the fourth, on the
accelerator that runs the guest ~50x faster between the host's two QMP writes.
The owner's rule names what this is — "everything should work under emulation and
kvm if it doesnt something with the guest is wrong" — and it is the class `specs/issues/hardware/`'s
xHCI entries already track: one outstanding operation per controller, a
completion matched by its Command TRB address, a recovery cancelled by a
disconnect. Not diagnosed further here; found by CI, which is the thing CI was
built to do.

**It has stopped reproducing, which is not the same as being fixed.** Run
`31258202923`, five reps of the whole twelve-shard configuration on the same
image and the same accelerator: `xhci_flap` is **PASS 5 of 5, in 7–9 s**. Nothing
in `toyos_xhci` changed between the two runs; what did change is `wt/toyos-clock`,
which replaced flat host waits with waits bounded by the guest, so one candidate
is that the probe's 164 s was a harness ceiling on a slow re-enumeration rather
than a wedge — and the log above, where the guest goes silent after the fourth
pair and never speaks again, is evidence against that reading. Left OPEN: a
defect that stopped appearing under an unchanged driver is a defect whose trigger
nobody has named. Re-read this entry from the log quoted above, never from a
green run.

**And it is not alone.** Run `31247206462`, twelve shards on KVM at `--jobs 1`,
put four more of its shape on the list, every one red again when re-run alone:

| test | what it does over QMP | alone |
|---|---|---|
| `xhci_hotplug` | `device_add`/`device_del`, fixed waits against a 100 ms debounce | `timed out after 66s` |
| `xhci_hid_break` | a staged transfer error on a HID endpoint | `timed out after 75s` |
| `metal_sim_pointer_churn` | 8 plug/unplug cycles under a live compositor | `bound 0 pointer sources` |
| `usb_transport_break` | one staged break, and the driver's first-try recovery | `the transport broke 2 times` |

All four are green on the dev host in seconds and green under TCG on the same
runner image and the same QEMU. So the class is one sentence: **the xHCI driver's
plug, unplug and endpoint-recovery paths are unreliable when the guest executes
natively**, and the ~50x speed-up between the host's two QMP writes is the only
thing that changed. `specs/issues/hardware/`'s own entries name the shape it would be in — one
outstanding operation per controller, a completion matched by its Command TRB
address, a recovery cancelled by a disconnect rather than waited out.

**This class is what stands between CI and green.** It is not `specs/issues/design-debt/`'s
classification class: it fails alone, and it fails the same way twice.

**Three of the four have stopped and one has not.** Run `31258202923`, five reps
of the whole configuration on the same image and accelerator: `xhci_hotplug`,
`xhci_hid_break` and `metal_sim_pointer_churn` are **0 of 5** — the last of those
is closed above and the other two coincide with `wt/toyos-clock`'s waits — while
**`usb_transport_break` is 5 of 5** with the same sentence it has always given,
`the transport broke 2 times; the injection is armed once per boot, so anything
else is a break this test did not stage`. That is the one member of this class
that is still reproducible, and it is the one to take: it is not a ceiling, it is
the driver reporting a second break nobody staged.

**Taken, and it was the driver.** Closed: the Bulk-Only Reset raced the transfer it was recovering from — see `reset_recovery` in `kernel/src/drivers/xhci/wait/msc.rs` and `toyos-xhci/src/recovery.rs`.
