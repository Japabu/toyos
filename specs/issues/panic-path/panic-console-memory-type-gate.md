---
status: open
kind: defect
opened: 2026-07-31
---

# The panic console's memory-type gate checks only the framebuffer's first byte

`kernel/src/drivers/panic_console/mod.rs:292-294`'s `framebuffer_is_reclaimed_ram`,
`maps.iter().find(|e| phys >= e.start && phys < e.end)`, classifies the entry
holding the scanout's first byte and ignores the rest of the range. A firmware
map whose scanout starts in `MemoryMappedIO` but whose tail falls in a
`BootServicesData` entry the PMM later hands out passes the gate, and the
panic-path write lands in the heap — the one outcome the gate exists to make
impossible. Checking every entry overlapping `[phys, phys + size)` is the same
loop. Untestable in QEMU (its map is well-formed); a T14 firmware-map hazard,
so fix it before the first metal boot.
