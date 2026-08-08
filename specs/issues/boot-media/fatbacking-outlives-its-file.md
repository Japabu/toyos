---
status: open
kind: defect
opened: 2026-08-02
---

# A `FatBacking` outlives the file it names, exactly as `/home`'s does

`FileSystem::delete` on this mount drops the *write* handle unconditionally, so
a `write_page` through an fd held across an unlink returns `"file not open"`
rather than putting one process's bytes into another's clusters — which is more
than the bcachefs adapters do. The read side is unchanged and shares `specs/issues/isolation/`'s live
cross-process leak: an `Arc<FatBacking>` already handed to the file cache still
names byte ranges the allocator is free to reissue.
