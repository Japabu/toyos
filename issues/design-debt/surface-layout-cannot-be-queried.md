---
status: open
kind: finding
opened: 2026-08-03
---

# Nothing can ask which layout a *surface* is translating with

Half of this closed with the input rework and the half that is left changed
shape. `SYS_SET_KEYBOARD_LAYOUT` is deleted; the layout is
`toyos::surface::LAYOUT_CONFIG`, a file, and anything may read it — so `locale`
could print the configured name today, and the interactive menu could open on
it. That is a small piece of work nobody has done.

What no file can answer is what each *translator* is actually using. There is
one per surface and each re-reads the config when its host says so, so a
terminal that missed the notification disagrees with the file and nothing can
see it. One query syscall answering "what is this process holding" is still the
shape that closes it, and it is still not built
(`issues/diagnostics/the-kernel-keeps-nothing-it-enumerates.md`).
