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

**The half of this that was somebody else's landed on 2026-08-20, and the
borrow did not.** `vma_map` used to pass `writable = true` unconditionally, so a
`LibMemory::Shared` image one process already had mapped was writable by that
process while `dlopen` relocated the same image for another. W^X closed it:
`mm::paging::Prot` has no writable-and-executable variant, and
`LoadedLib::map_into` maps a module's code `ReadExec` in every process that
loads it, so no process can write the cached image at all.

What is left is the borrow itself, and it is not a userland question. The kernel
writes that same memory through the *direct map* while the `&[u8]` exists —
`load_shared_lib`'s relocation pass does exactly that, and a user mapping's
protection says nothing about it. So this is now the whole of this file rather
than half of somebody else's, and it is still a `&[u8]` aliasing a write.
