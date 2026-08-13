---
status: open
kind: defect
opened: 2026-08-03
---

# Two framebuffer clients still pay the scanout's price, and the panic console pays it worst

Closed for `/bin/console` in `45b5010`: the terminal emulator composed against
the panel, so `Framebuffer::scroll_up`'s `ptr::copy` read back every byte it
moved — 16,777,212 bytes read and 16,820,337 written per scrolled text row,
counted in QEMU at 2048x2048. It composes in system RAM now and blits damage
through `window::Screen`, which has no read path at all. What follows is what
that left behind.

**The compositor holds the raw scanout as a `Framebuffer` and reads it.**
`userland/compositor/src/main.rs:740` builds one over the GOP mapping, and two
paths read through it. `draw_software_cursor` (`:719`) calls `get_pixel` per
cursor pixel to blend against what is underneath. And `Framebuffer::fill_rect`
copies its first row to every subsequent row, so a full-screen `clear` reads
back one row short of the whole surface — the 16.7 MiB read the console's
startup used to spend, measured before the fix. Neither shows up under QEMU,
where the framebuffer is host RAM. The fix shape is the console's: compose in
system RAM, hand `Screen` finished pixels. It is a larger job there because the
compositor's damage model is per-window rather than per-cell.

The compositor now says what those paths move, from inside. Every ~2 s, from a
composited frame, `FrameStats::report` prints `scanout_rd_bytes` — split into
`rd_px`, the cursor's `get_pixel` calls, and `rd_bulk`, `fill_rect`'s row
copies — beside `scanout_wr_bytes`, the frame count and min/max/total composite
time. Closing this entry is what takes all three read figures to zero.
`metal_sim_compositor` requires the line and prints it: three frames at
1920x1080 read 747,144 bytes back for 9,531,840 written, reproducing
byte-for-byte between runs while the times do not. They are byte counts and
never a cost — the cost is the uncached read, which QEMU cannot have. Both
figures are lower bounds, and anyone deriving from them needs to know by how
much: they count what goes through `Framebuffer`, so glyphs (`put_pixel`,
uncounted by design) and the title-bar icons (handed `screen.ptr()`, and
alpha-blending through it, so they read the panel as well as write it) are in
neither. The 1920x1080 figures above have no window on screen and therefore no
icons; a desktop's do.

**The panic console's repaint is ~460 ms on the T14**, measured from inter-line
gaps in both boot logs (461 ms and 459 ms) in
`specs/reference/metal-hardware-inventory.md`; five of the six `boot_checkpoint`
repaints fall inside the reported 3422 ms boot, which is most of it.

This is *not* the same defect. `kernel/src/drivers/panic_console/mod.rs` never
reads the framebuffer — it writes `core::ptr::write_volatile` one `u32` at a
time, in `fill_screen` (`:769`) and per glyph bit (`:791`). 1920x1080 is
2,073,600 of those per full repaint, which at 460 ms is 222 ns each: the cost
of a store to an uncached mapping.

**The mapping is write-combining now, which changes what is left of this.** The
kernel programs `IA32_PAT` entry 4 to WC on every CPU and maps the scanout
through it — its own direct map and every process holding a token — so a store
to the panel merges with its neighbours instead of being its own bus
transaction. `fill_screen` writes a row of `u32`s in address order and is what
gains most; `draw_glyph` writes one `u32` per set bit across sixteen rows and
gains least, because scattered stores are what WC cannot merge. Neither figure
exists: QEMU's framebuffer is host RAM and can show neither, so what is open
here is the measurement off the T14 rather than the mechanism.

That makes the painter's granularity the live question again — a glyph
assembled in a scratch row and blitted as one run would merge where the per-bit
writes do not. Note the constraint that rules out the obvious scratch buffer on
the panic path: it takes no lock of any kind and paints from contexts where
nothing may be waited on, so a shared static strip would add exactly the
multi-CPU race `specs/issues/panic-path/` already records against `capture()`.

Belongs to #65 (boot time) rather than to the console work that found it.
