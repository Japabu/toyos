---
status: open
kind: defect
opened: 2026-08-03
---

# A device still plugged in after a refused enumeration keeps its slot, on eleven paths

**A slot is now given back when its device is unplugged, and only then.**
`0ed2bc1` added Disable Slot and made `device::configure` carry the slot id out
of *every* path below the successful Enable Slot, including the eleven refusals
below — so the port remembers the slot whether or not a device came of it, and
`teardown_port` disables it. `xhci_hotplug` shows the controller handing the
same slot id straight back to the next device plugged into that controller.

What that closes is the hotplug half, which was the half that grows: without it
every plug cycle cost a slot and 64 of them exhausted a PCH controller. What it
does not close is a device that is **still plugged in** after a refused
enumeration — a hub, a camera, a fingerprint reader, or any of the eleven paths
— which keeps its slot until it is pulled. That is the rest of this entry,
unchanged, and the count is still 11.

`init_device` enables a slot for every connected port and issues no Disable
Slot, on any path: not for the devices it walks past (a hub, camera or
fingerprint reader), not when Address Device fails, not when the descriptor
fetch fails, and not when the slot id comes back past the pool's device blocks
(the `layout.device()` `None` branch). Each of those keeps a slot for a device
the driver will never talk to again. Mass storage added three more: a disk
whose interface has no bulk pair, one the pool has no mass-storage block for,
and one that fails `bring_up` — and the boot stick came *off* the list, since
it now binds.

The fourth is the one with a test behind it: `xhci_slot_exhaustion` leaves five
slots enabled with a zero DCBAA entry every run, which makes the entry's own
test the largest producer of the leak it describes.

**The count is 11, not four plus three**, enumerated in
`specs/assessments/type-safety-audit/usb-storage.md` F12 by reading every path between the
successful Enable Slot and a bound device. Three of them are named nowhere
else: SET_CONFIGURATION failing (`device.rs`), Configure Endpoint failing for
the bulk pair (`msc.rs`) and for the HID interrupt endpoint (`device.rs`), plus
`PointerSource::claim` running out. A fix that adds Disable Slot to the four
named above leaves seven behind, and this entry is what somebody will work
from.

Harmless where slots outnumber ports, which is every machine in reach: QEMU
reports 64, Intel's PCH controllers 32 or more, and no root hub has that many
ports. It stops being harmless on a controller whose slot count is below its
device count, where a HID on a later port loses its slot to a hub on an earlier
one. `xhci_slot_exhaustion` is what would catch the regression — it proves the
machine survives the shortage and that the one device which fit was enumerated
to completion, not that the right devices win it.
