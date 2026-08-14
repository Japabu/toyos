---
status: open
kind: finding
opened: 2026-08-08
---

# Four configs ship doom's assets into initrds holding nothing that can open them

`assets = ["assets"]` sweeps the directory whole and there is no way to name
part of it, so `console/`, `tests/desktopcase`, `tests/desktopaudiocase` and
`tests/metalcase` each carry `DOOM1.WAD` (4,196,020 B) and now
`soundfont.sf2` (15,546,764 B) into an image with no doom in it. This is
`specs/plans/boot-image-split.md` §5's shape, four times bigger: that section records
the same four configs paying 5,994,284 B for TimGM6mb when
`untracked-assets` declared it in configs that did not build doom.

**Measured before it was left alone**, 2026-08-08, one session, same worktree:
`metal_sim_compositor` is 8 s either way, and the harness's own boot probe moves
from 1,445 ms to ~1,485 ms — about 40 ms of boot for 15.5 MB of initrd. The
flashable `--console-boot` image grows by the same 15.5 MB, which matters more
to whoever writes it to a stick than to the suite.

The fix is per-config asset selection — `assets` naming files as well as
directories — and it changes what five configs ship, under screen tests that
read pixels off four of them. Not worth 40 ms; worth doing when something else
touches that code.
