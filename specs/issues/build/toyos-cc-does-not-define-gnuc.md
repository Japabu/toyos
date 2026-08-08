---
status: open
kind: finding
opened: 2026-08-05
---

# `toyos-cc` does not define `__GNUC__`, so doomgeneric compiles unpacked

`PACKEDATTR` in `userland/doom/include/doomtype.h` and in doomgeneric's own
`doomtype.h` is `__attribute__((packed))` under `#ifdef __GNUC__` and empty
otherwise, and toyos-cc seeds neither `__GNUC__` nor `__GNUC_MINOR__`. Measured:
preprocessing `w_wad.c` through toyos-cc yields zero occurrences of either
`PACKEDATTR` or `__attribute__`, and `} PACKEDATTR wadinfo_t;` arrives at the
parser as `} wadinfo_t;`.

This is inert today and was checked rather than assumed. Compiling doomgeneric's
fourteen `PACKEDATTR` structs with clang twice, once with the macro empty and
once with it expanded, moves **no field offset at all** and changes one size:
`pcx_t` is 130 unpacked and 129 packed. `WritePCXfile` never takes
`sizeof(pcx_t)` — it writes the header field by field and derives the length
from its own pack pointer, and `offsetof(pcx_t, data)` is 128 either way. The
remaining thirteen differ only in alignment, and every one of them is read
through a pointer into a WAD buffer.

Defining `__GNUC__` would be a much larger change than it looks: it turns on
every `#ifdef __GNUC__` block in doomgeneric and in any header that has one.
