---
status: open
kind: finding
opened: 2026-08-01
---

# USB mass storage: what is not implemented

The driver serves one logical unit per device and speaks the SCSI commands a
disk needs. Deliberately absent, each with its reason:

- **Multiple LUNs.** `GET MAX LUN` is not issued and `bCBWLUN` is always 0. A
  card reader with four slots presents four LUNs and this would see the first.
- **UAS** (USB Attached SCSI, protocol 0x62). A modern enclosure advertises
  both; the driver takes the BOT interface, which every such device still
  offers. UAS is a different transport with its own streams support in the
  endpoint context.
- **CBI/CB** (subclass 0x00–0x05, protocol 0x00/0x01). Floppy-era transports.
- **READ(16)/WRITE(16).** The driver refuses a device whose last LBA does not
  fit 32 bits rather than serving its first 2 TiB. `READ CAPACITY(16)` *is*
  implemented, because it is how such a device reports the size that gets it
  refused — and `Profile::UsbDiskHuge` is the only place either runs.
- **Removable media.** No `PREVENT ALLOW MEDIUM REMOVAL`, no unit-attention
  handling beyond the `REQUEST SENSE` that clears it during bring-up. A card
  swapped under a running system is not noticed.
- **MODE SENSE.** Write-protect is discovered by a WRITE failing, not in
  advance.
- **Concurrency.** One command at a time per controller, under the xHCI lock,
  with preemption disabled for its duration. Fine at boot; a filesystem doing
  real I/O over USB will want the queue depth the transfer rings already allow.
