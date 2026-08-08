---
status: open
kind: defect
opened: 2026-08-07
---

# The page cache owns one device, and `usb_storage.rs` says it does not

`page_cache.rs:11-12` holds exactly one
`BLOCK_DEV: Lock<Option<Box<dyn BlockDevice>>>`, `page_cache::init` takes
ownership of the NVMe device, and `PageCache::_device_id` is written at
construction and read nowhere. So `usb_storage.rs:14-17`'s comment — *"NVMe
takes 1; the page cache keys itself on this, so two devices sharing a number
would serve each other's blocks"* — describes a mechanism that does not exist.
The numbers are right and the keying is not.

`fat32_adapter.rs:911-915` states the live consequence and does not work around
it: a machine that boots off an **internal** disk gets neither `/boot` nor
`/log`, "because the NVMe device is owned by the page cache from the moment
storage comes up and there is no second handle to it". `/boot` and `/log` work
on the T14 and in QEMU only because the boot medium is USB, and
`usb_storage::open` mints a fresh handle per call.

This is the real cost of `specs/boot-image-split.md` stage 2: a bcachefs root on
the boot medium needs a `BlockIO` over an arbitrary `BlockDevice` at a partition
offset with a cache of its own, where `PageCacheBlockIO` *is* the NVMe device by
construction. Found 2026-08-07 while pricing that stage — the 2026-07-29 version
of that document listed this as one of eight items a USB storage driver would
have to bring, and it is the one that did not arrive with it.
