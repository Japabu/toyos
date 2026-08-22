---
status: open
kind: defect
opened: 2026-08-22
---

# `InitrdBacking` cannot bound an extent against the initrd, because it is never told the image's size

`kernel/src/file_backing.rs`:

```rust
pub struct InitrdBacking {
    initrd_base: *const u8,
    extents: Vec<Extent>,
    size: u64,          // the FILE's size, not the image's
}
```

`file_offset_to_ptr` computes
`initrd_base.add(initrd_block * BLOCK_SIZE + off_in_block)` and nothing anywhere
checks that address against the end of the initrd. `size` is the size of the
*file* being read, which bounds how many bytes are copied out of the block but
says nothing about where the block is.

`extents` comes from the bcachefs btree **inside that same image**
(`bcachefs_adapter.rs` builds all three `InitrdBacking`s from extents it walked
out of the mounted initrd), so a corrupt or hostile image names blocks past its
own end and the kernel reads whatever the bootloader placed after it. The read
lands in `read_page`'s `copy_nonoverlapping` — a kernel read out of bounds, from
metadata that has not been validated against anything.

## Why it is not fixed where it was found

The fix is a length this type does not carry: `InitrdBacking::new` would take
the image's own length beside `initrd_base` and `file_offset_to_ptr` would
answer `None` past it. That is three call sites in `bcachefs_adapter.rs`
(`initrd_base` is a field of the adapter's prober, which does know the length —
`mount_initrd` is handed `ptr` and `len` together) plus the constructor. The
`undocumented_unsafe_blocks` sweep of the kernel's root files (2026-08-22)
found it while writing the `SAFETY:` comment that now names the gap at the site.

## The related gap next to it

`bcachefs_adapter::mount_initrd(ptr: *const u8, len: usize)` takes a raw pointer
and a length and hands them to `SliceBlockIO::new` without being an `unsafe fn`
— the same pattern
`issues/kernel/raw-pointer-writers-not-marked-unsafe-in-loader.md` records for
`elf::read_backing_into` and `RelocationIndex::apply_to_page`. Its one call site
(`main.rs`, from `KernelArgs`) is correct; nothing enforces that the next one
is.
