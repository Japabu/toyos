# Kernel Technical Debt

Tracked issues to address, roughly ordered by impact.

## ~~1. Identity map~~ DONE

Removed. Kernel PML4 has no PML4[0..255] entries. Process page tables start
empty and build user mappings on demand. SMP trampoline uses the bootloader's
PML4 for AP transition, then switches to kernel PML4 in ap_entry.

## 2. user_ptr assumes physically contiguous user buffers

`SyscallContext::user_slice()` translates the first page of a user buffer and
returns a `&[u8]` spanning the full length. This works because ToyOS allocates
user memory in physically contiguous 2MB blocks. If we ever add swap, COW fork,
or non-contiguous VMAs, this breaks silently.

**Fix:** Replace `user_slice` / `user_slice_mut` with `UserBuf` / `UserBufMut`
types that copy page-at-a-time through the direct map. The types exist but the
syscall handler refactoring had bugs. Port handlers one at a time with testing.

**Not urgent while all user allocations are 2MB-aligned contiguous blocks.**

## ~~3. No DmaAddr newtype~~ DONE

Added `DmaAddr` newtype in addr.rs. `DmaPool::page_phys()` returns `DmaAddr`.
All drivers use `DmaAddr` for device-visible addresses. `Virtqueue::base_phys`
is `DmaAddr`. Every site that writes to hardware uses `.raw()` explicitly.

## ~~4. PhysAddr::from_ptr is unchecked~~ DONE

Added `debug_assert!` in `from_ptr()` that validates the pointer is in the
high-half direct map.

## ~~5. No compile-time enforcement of user pointer safety~~ VERIFIED OK

Audited all `UserAddr::raw()` call sites — no kernel code outside `user_ptr.rs`
and `paging.rs` creates raw pointers from user addresses. The convention is
already clean. `UserAddr::raw()` is needed for page table walks and address
arithmetic, but no code dereferences the raw value.

## 6. Large NVMe reads for demand paging

The demand pager maps 2MB pages but populates them by issuing up to 512
individual 4KB page cache lookups (each potentially a separate NVMe read).
This works but is suboptimal — a single NVMe command can transfer 2MB in
roughly the same time as a 4KB read (command overhead dominates, not transfer
size).

**Fix:** Add a bulk read path: when demand-paging a 2MB region, issue one
NVMe read for the entire 2MB range and populate 512 page cache entries at
once. The page cache stays at 4KB granularity (important for fine-grained
eviction of cold blocks), but the I/O path batches reads.

**Not urgent:** Current startup times are fine. This is a throughput
optimization for large binaries.

## 7. Demand paging called from user_ptr translate()

When `virt_to_phys` returns None, `user_ptr::translate()` calls
`process::handle_page_fault()` directly. This works but is fragile — if
`handle_page_fault` ever needs locks that are held by the caller, it deadlocks.
The old approach (kernel dereferences user pointer, CPU faults, fault handler
maps page, instruction retries) was more robust.

**Fix:** Either (a) ensure all user pages are faulted in before syscall entry
(a "prefault" pass), or (b) accept the current approach but audit
`handle_page_fault` to ensure it never takes locks that syscall handlers hold.
Option (a) is cleaner but slower for large buffers.
