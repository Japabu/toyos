---
status: open
kind: defect
opened: 2026-08-08
---

# Anonymous `mmap` is not demand-paged, and `.bss` is written into the file

Two claims this tree makes about itself that its own code does not keep. Found
by `wt/toyos-fpu` while building a page-fault workload for `fpu_isolation`, and
recorded rather than fixed: both are memory-subsystem changes with their own
blast radius.

**`sys_mmap` allocates and maps the whole region up front.** `PageAlloc::new` is
called for the full rounded size before anything is mapped, and
`alloc_and_map` maps every 2 MiB page of it (`kernel/src/arch/syscall.rs`'s
`sys_mmap`). So a first touch of a fresh anonymous mapping is an ordinary store
and never a `#PF`, and a program that reserves a large region pays for all of it
immediately. Measured: `syscall_cost` mapping 128 MiB and touching one byte per
page reported the guest's `peak=130MB` and its fault trace named two faults, both
in the ELF. CLAUDE.md's "Demand paging" is true of a *file-backed* segment and
of nothing else. Whether it should be lazy is a design question with a real
answer either way — the eager path is simpler and cannot fail late — but the
description and the code should agree.

**`toyos-ld` materializes `.bss` into the file.** A `static mut [u8; 16 MiB]`
that is all zeroes produced an 18 MB binary, and a 128 MiB one produced a
135 MB binary and a 465 MB initrd; both are in the same run's `initrd: adding`
lines. A `PT_LOAD`'s `filesz` therefore equals its `memsz`, which also means the
loader's `Anonymous` tail (`loader/mod.rs`, `file_backed_end < seg_end`) is
always empty — that branch exists and nothing in the tree can reach it. Zeroed
pages a linker never has to write are the whole point of `.bss`, and this is
paid in image size, in initrd size, and in every copy of both.

The one demand-paged thing a userland program can still reach is a *writable
file-backed* page — `demand_paging_sse` and `fpu_isolation` both use one — at
2 MiB of test image per fault.
