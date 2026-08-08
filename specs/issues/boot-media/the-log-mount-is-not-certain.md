---
status: open
kind: defect
opened: 2026-08-01
---

# The mount is not certain, and one failure is unexplained

Across the boots recorded while this was built, `esp: no boot volume` (as the
line then read) appeared
on a handful. Two instances are explained and closed: `gpt: device 16 has no
partition table we can use: EntryArrayCrc { … }`, which is a *read* off the
stick coming back wrong, from the window where `BlockDevice::read_blocks`
returned `()` and `DeviceSectors::read_lba` served the previous block's bytes
under the new block's tag — closed at `3c5a7b8` and `kernel/src/gpt.rs`'s cache
now drops the tag with the read. One instance after that fix is unexplained,
because the failing run was not captured with serial output. `esp_lines` now
includes `gpt:` and `usb-storage:` lines in the failure message, so the next
one will say which it is.
