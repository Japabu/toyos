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

## 3. No DmaAddr newtype — physical/virtual confusion in drivers

Driver code passes physical addresses to devices as raw `u64`. Nothing at the
type level prevents accidentally passing a virtual (high-half) address to a
device descriptor. This was a source of bugs during the higher-half migration.

**Fix:** Add `DmaAddr(u64)` newtype that can only be created from `PhysAddr`
or `DmaPool::page_phys()`. Device descriptor structs use `DmaAddr` instead of
`u64`. The compiler rejects `*mut u8` or high-half addresses in descriptor
fields.

## 4. PhysAddr::from_ptr is unchecked

`PhysAddr::from_ptr(ptr)` subtracts PHYS_OFFSET from any pointer. If `ptr` is
not a direct-map address (e.g. a user address or null), the result is garbage.
No debug assertion catches this.

**Fix:** Add `debug_assert!(ptr as u64 >= PHYS_OFFSET)` in `from_ptr()`. In
release builds it's a no-op. In debug builds it catches misuse immediately.

## 5. No compile-time enforcement of user pointer safety

The `user_ptr` module is the intended gatekeeper for user memory access, but
nothing prevents kernel code from casting a `u64` to `*mut u8` and
dereferencing it directly. This bypasses validation, SMAP protection (if we
ever need it), and the page-table-walk translation.

**Fix:** Make `UserAddr` truly opaque — remove any method that produces a raw
pointer. The only way to access user memory is through `SyscallContext`. This
is already the convention; making it a hard constraint means the compiler
catches violations.

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
