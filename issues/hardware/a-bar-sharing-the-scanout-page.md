---
status: open
kind: defect
opened: 2026-08-04
---

# A BAR sharing the scanout's last 2 MiB page is a boot panic

`map_2m` refuses to put two different non-default memory types in one 2 MiB
page: whichever call ran last would decide, and the other would write through a
type it did not ask for — which for a device BAR means combined, reordered
register writes. The refusal is a panic naming both entries.

That is the right failure and it is reachable from firmware's BAR layout rather
than from any kernel bug. The scanout is mapped write-combining by
`panic_console::remap`, which runs before every driver's `map_mmio`, so a BAR
placed inside `[fb, fb + align_2m(fb_size))` panics the boot. The layout that
would do it is a small BAR immediately after the framebuffer's, close enough to
land in the same 2 MiB page — which BAR alignment rules make unlikely, since a
framebuffer BAR is large and the next one starts on its own size boundary, but
does not forbid.

Not staged on either machine: QEMU's stdvga puts nothing there, and the T14 has
not been asked. If a T14 boot ever panics in `map_2m` naming `0x4000000000`,
this is what happened, and the fix is to give that page `DeferToMtrr` and take
the framebuffer's last page back to UC rather than to widen the check.
