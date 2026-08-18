---
status: none
kind: rejected
opened: 2026-08-05
---

# Defining `__GNUC__` is declined: it would claim a GNU C this compiler is not

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

**Declined on the merits.** Defining `__GNUC__` is a claim to implement GNU C,
and toyos-cc stops on a long list of GNU constructs: some refused by name
(`__attribute__((cleanup))`, `((constructor))`, `((alias))`, file-scope `asm`,
`#pragma pack`), some as parse errors (`asm goto`, `_Alignas`). Seeding the
macro turns on every `#ifdef __GNUC__` block in every header at once and hands
all of it to a compiler that will refuse most of it — the attribute ruling read
backwards, where the defect is claiming a capability you do not have. The
measurement above shows it buys nothing today, so there is no gain to weigh
against that, and nothing is owed once it is declined.
