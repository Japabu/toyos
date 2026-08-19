---
status: open
kind: defect
opened: 2026-08-07
---

# `debug = true` produces no debug info, because the linker drops it

`toyos-ld` matches `SectionKind::Debug | DebugString | Linker | Note | Metadata`
and `continue`s (`collect.rs:410-416`), so **no binary this project produces has
a `.debug_*` section**. Verified with `readelf -S` on the kernel, the compositor
and toybox: the sections are `.text .strtab .symtab .rela.dyn .data
.eh_frame_hdr .dynamic .shstrtab` and nothing else.

`[profile.toyos]` states `debug = true` in every crate root, so rustc emits
DWARF into every object file and the linker throws all of it away. The cost is
compile time and has not been measured. The consequence for diagnostics is that
a backtrace can carry a **name** and never a line number or an inlined frame, on
any path — `.symtab`/`.strtab` is the whole of what survives, and it is 32.2% of
the 92,138,384 bytes of ELF this tree ships. Keeping `.debug_line` in `toyos-ld`
is what would change that, and it is not planned.
