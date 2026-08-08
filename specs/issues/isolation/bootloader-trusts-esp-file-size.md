---
status: open
kind: defect
opened: 2026-07-31
---

# The bootloader sizes every allocation from a file the ESP handed it

`bootloader/src/main.rs:61-62` reads the UEFI-reported `file_size()` and
allocates that much for the kernel and the initrd, with no bound.
`:103,112` takes `max(p_vaddr + p_memsz)` over the kernel ELF's segments with no
overflow check and allocates it, then `:122` copies `p_filesz` bytes into that
`p_memsz`-sized buffer without checking `filesz <= memsz` — the kernel's own
`toyos_elf::Layout::parse` enforces that, the bootloader does not — and it is
now a crate the bootloader could depend on, which is the cheapest form this fix
will ever take.

Lower severity than the kernel-side entries here: it runs before ExitBootServices,
on files we put on the ESP ourselves. It is on the list because the metal track
makes the ESP a thing a user can write to, and because none of the kernel's
protections exist yet at that point.
