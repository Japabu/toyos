---
status: open
kind: defect
opened: 2026-08-07
---

# `PciDevice::read_bar_64` cannot see an I/O BAR, and reads the next register as one

`kernel/src/drivers/pci.rs:118`. It takes bits 2:1 of the low dword as the
Memory Space BAR's Type field without first checking bit 0, which is the bit
that says whether the register describes memory at all.

On an I/O BAR bit 0 is set, bit 1 is reserved, and **bits 31:2 are the port
number** — so bits 2:1 read as `(0, address bit 2)`. A port whose bit 2 is set
therefore decodes as Type `0b10`, the 64-bit encoding, and the function reads
the *next* BAR register as the upper half of an address. The other half of the
same defect is quieter: with bit 2 clear it returns the port number with the low
nibble masked off, as a physical address. There is no encoding of an I/O BAR
this function refuses.

Nothing reaches it today. `read_bar_64(0)` on an NVMe or xHCI controller and the
BARs a virtio capability names are memory BARs on every part that exists;
`enable_msix` is the one caller whose index comes out of a device-supplied
field, and that path now refuses the reserved indicators and an unassigned BAR
(`toyos-pci::msix`) — but not an I/O one, because the type is not in the
register it decodes.

The fix is a typed BAR decode beside those, and it changes the signature: a
caller that wants memory has to be handed an `Option<u64>` and say what it does
without one. Four call sites in three drivers, one of them in `xhci/`.
