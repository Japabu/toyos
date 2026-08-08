---
status: open
kind: defect
opened: 2026-07-30
---

# `SYS_DLOPEN` never dedups and `SYS_DLCLOSE` is a no-op

A process can exhaust its virtual address space by repeated loads of the same
library. The *panic* is closed — `syscall.rs:1435`/`:1446` no longer `.expect` —
but the unbounded VA growth is not, and `SYS_DLCLOSE` (`syscall.rs:298`) still
frees nothing.

Deliberately left by the ELF-hardening pass rather than missed. Dedup is a
semantic change, not a bounds check: a second `dlopen` of a loaded library would
return a handle sharing the first module's id and TLS block, and
`std_tls_dlopen`'s test 10 exercises exactly that case. It needs its own change
with its own test, not a hardening drive-by.

Left alone again by the `toyos-elf` extraction, for the same reason and one
more: the whole change is inside `sys_dlopen`'s arm, where the handle is minted
and where the process's module list lives, and that arm is `arch/syscall.rs`'s.
The extraction touched two lines of it. Whoever takes this owns the *handle*,
not the loader.
