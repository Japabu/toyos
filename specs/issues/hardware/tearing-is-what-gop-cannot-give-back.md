---
status: open
kind: finding
opened: 2026-08-03
---

# Tearing is what GOP cannot give back, and two clients still present whole

Recorded from the compositor's rendering pass (task #132), which moved
composition into system RAM and gave `MSG_PRESENT` a damage rect. Three
residuals, none of them fixed there:

**The scanout blit is not synchronised to the panel.** Nothing composes onto
the scanout any more, so a frame is never seen half-composed; what remains is
that the blit of a damage rect can land while the panel is reading those rows,
so that rect can tear. The window shrank from "a whole composite" — a wallpaper
blit, then every window, then the taskbar, then the cursor, each a separate
pass over the mapping — to one `memcpy` of the damaged rows. Closing it needs
to know where the beam is, and GOP does not say: there is no vblank interrupt,
no scanline register and no page flip in the protocol. It is the display
driver's to close, and the owner's ruling is that the driver comes later and
whole. Double-buffering does not help without the flip either — with one
scanout and no way to swap it, a second buffer is what the back buffer already
is.

**Every window client except the terminal presents its whole window.** `paint`,
`files`, `editor`, `filepicker`, and everything on winit/softbuffer (doom,
snake) draw into their shared buffer through a readable `Framebuffer` and keep
no record of where, so `Window::present()`'s "all of it" is the honest answer
for them and `present_damage` is unused. The terminal is the one that composes
through a `window::Screen` and can therefore be asked what it painted. Each of
the others is the same shape of change: compose through `Screen`, hand
`take_damage()` to `present_damage`.

**The back buffer is one screen of RAM charged to nobody.** 8.29 MiB at
1920x1080, on top of the window buffers `specs/issues/isolation/`'s window-cap note already covers.
Same root: there is no per-process memory limit, no pressure signal and no OOM
killer, so a compositor's own working set is bounded by nothing but its code.
