---
status: open
kind: defect
opened: 2026-08-19
---

# Four more private 4 KiB constants, now that one has somewhere to go

`issues/design-debt/four-deletions-still-owed.md` named five private 4 KiB
constants with no common owner and said whoever fixed `block.rs`'s copy had to
make the export first. That happened: `kernel/src/mm/mod.rs` now has
`pub const PAGE_SIZE: u64 = 4096;`, next to `PAGE_2M`, and `block.rs:73`'s
copy is gone.

The other four are still private, still `4096`, still unpinned to the export
or to each other:

```
$ grep -n '^const PAGE_SIZE\|^const BLOCK_SIZE\|^const BLOCK_SIZE_U64\|^const BLOCK:' \
    kernel/src/file_cache.rs kernel/src/file_backing.rs kernel/src/fat32_adapter.rs kernel/src/usb_gate.rs
kernel/src/file_cache.rs:13:const PAGE_SIZE: usize = 4096;
kernel/src/file_backing.rs:9:const BLOCK_SIZE: usize = 4096;
kernel/src/file_backing.rs:10:const BLOCK_SIZE_U64: u64 = 4096;
kernel/src/fat32_adapter.rs:75:const BLOCK: u64 = 4096;
kernel/src/usb_gate.rs:31:const BLOCK: usize = 4096;
```

Not a pure rename: `mm::PAGE_SIZE` is `u64`; `file_cache.rs`'s and
`usb_gate.rs`'s copies are `usize`, and `file_backing.rs` keeps both widths
side by side under two names precisely because its callers want both. Whoever
does this picks up the `as` casts (or the `usize`/`u64` split) at each site
rather than assuming the types already line up.

Filed while resolving the block.rs half of `four-deletions-still-owed.md`;
verified at the commit that added `mm::PAGE_SIZE`.
