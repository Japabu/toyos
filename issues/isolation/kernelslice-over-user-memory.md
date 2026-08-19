---
status: open
kind: defect
opened: 2026-08-07
---

# `KernelSlice` is the last `&[u8]` over memory userland can write

`user_ptr` hands out no reference to user memory any more, but that is a
statement about addresses *userland chose*. `mm::region::KernelSlice::as_slice`
(`mm/region.rs:52-54`) is
the other direction: a kernel allocation the loader later maps into a process,
so the borrow is created before the aliasing exists. `elf.rs:973` builds a
`&str` out of one — a `dynstr` symbol name read during relocation.

Whether it is reachable depends on #159, not on this. `vma_map` passes
`writable = true` unconditionally, so a `LibMemory::Shared` image one process
already has mapped is writable by that process while `dlopen` relocates the
same image for another. Fixing the protection — `Protection` as a type,
`issues/kernel/every-user-mapping-is-writable-and-executable.md` — closes the
aliasing and makes the borrow honest; converting the borrow first would describe
a hazard that fix removes. Recorded so the two are known to be one question
rather than two.
