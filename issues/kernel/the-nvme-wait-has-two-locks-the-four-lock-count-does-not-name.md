---
status: open
kind: finding
opened: 2026-08-20
---

# The NVMe wait is held under two locks that "four locks and there is no fifth" does not count

`issues/kernel/every-wait-in-this-kernel-is-a-spin.md` records, as a decision
not to be re-argued: *"Four locks convert and there is no fifth."* The four are
`vfs::VFS`, `fat32_adapter::VOLUMES`, `xhci::XHCI` and `process::ProcessData`.

That is true of the **xHCI** path, and it was checked on 2026-08-20 while
landing wall 3:

```
syscall → vfs::VFS → fat32_adapter::VOLUMES → xhci::XHCI → wait_transfer
```

`FatDevice` owns its `Box<dyn BlockDevice>` outright (`fat32_adapter.rs`), so a
FAT read never touches the page cache and the chain really is three locks plus
the process table.

It is **not** true of the **NVMe** path, which the same track names as one of
its four spinning wait sites:

```
… → page_cache::BLOCK_CACHE → page_cache::BLOCK_DEV → NvmeDisk::read_blocks
    → Queue::wait_completion
```

`page_cache.rs` holds `BLOCK_DEV` across the whole of `read_blocks` in
`raw_block_read`, `raw_block_write` and `PageCacheGuard::cache_and_dev`, and
`PageCacheGuard` holds `BLOCK_CACHE` above it — its own comment states the order
("Lock ordering: BLOCK_CACHE → BLOCK_DEV, never reversed"). Both are
`sync::Lock`, so both disable preemption for the whole device round trip, and
`Queue::wait_completion` (`drivers/nvme.rs:117`) is a bare
`loop { …; core::hint::spin_loop() }` with **no deadline in it at all** — not
even the `USB_TIMEOUT_NS` the xHCI side has.

So the count is right for the chunk it was written for and wrong as a statement
about the kernel. Whoever converts the NVMe wait inherits two more statics, and
neither is a leaf: `BLOCK_CACHE` guards a `HashMap` and a slot vector that every
btree walk touches, and `BLOCK_DEV` guards a `Box<dyn BlockDevice>` whose trait
is the one that provably cannot carry a park token
(`scheduler::Operation` is why).

Not fixed here: the xHCI chunk converts no lock on this path and touches none of
these files. What is owed is one sentence on the track — six locks, four in this
chunk and two in the NVMe one — so the next reader does not take "there is no
fifth" as a property of the machine.
