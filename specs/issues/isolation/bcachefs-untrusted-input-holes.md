---
status: open
kind: defect
opened: 2026-08-01
---

# `bcachefs`: three residual untrusted-input holes and a mount-policy question

Left open deliberately, in the same crate:

1. **`decode_leaf_value` does not range-check an extent.** A file's
   `start_block` comes off the disk unchecked and reaches `read_extents` (via
   `read_link`, which *is* on the adapter) and `NvmeBacking`'s demand paging (via
   `file_extents`). With the child-pointer check removed, a `u64::MAX` block
   number reaches `BlockNum`'s byte-offset multiply and panics with "attempt to
   multiply with overflow" — measured, and the same multiply is what an extent
   reaches today with nothing in the way. `Extent.start_block` is a bare `u64`
   crossing the crate boundary into `kernel/src/file_backing.rs`, which is why.
2. **`read_extents` sizes `vec![0u8; size]` from the on-disk file size.** The
   honest bound is one line — a file cannot be longer than the blocks it names.
3. **`BlockNum::to_byte_offset` multiplies unchecked**, next to a `checked_add`.

And the policy question, for the owner, **not changed here**: `probe()` mounts
any disk whose block 0 carries `BCFS`, version 1, and a CRC32C that checks out.
A CRC is not authentication — whoever writes the image writes the CRC — so the
split is *a token naming this device authorises a format, a checksum anybody can
compute authorises a read-write mount*, and both actions write to the disk.
Detail and a recommendation are below, under "`probe()` mounts on a checksum".
