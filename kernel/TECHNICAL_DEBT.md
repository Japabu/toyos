# Kernel Technical Debt

Tracked issues to address, roughly ordered by impact.

## 1. Identity map still present in process page tables

Process PML4[0..N] deep-clones the kernel's identity map so kernel code can
execute at low addresses and demand paging can access user pages. This wastes
memory (every process clones ~8 page tables per GB of RAM) and means the
identity map entries can still get USER bit set by `map_user_in`.

**Fix:** Relink the kernel binary to a high-half virtual address. The bootloader
already relocates to PHYS_OFFSET — changing the link address makes kernel code
execute at high-half addresses natively. Then PML4[0..N] can be removed from
process page tables entirely: only PML4[256+] (shared kernel, no deep clone)
and per-process user mappings in PML4[0..255].

**Blocked by:** The kernel is PIE but linked at address 0. Relinking requires
either a linker script change or a fixed load address in the bootloader.

## 2. user_ptr assumes physically contiguous user buffers

`SyscallContext::user_slice()` translates the first page of a user buffer and
returns a `&[u8]` spanning the full length. This works because ToyOS currently
allocates user memory (stack, TLS, ELF segments) in physically contiguous
blocks. If we ever add swap, COW fork, or non-contiguous VMAs, this breaks
silently — the kernel would read/write the wrong physical pages.

**Fix:** Replace `user_slice` / `user_slice_mut` with `UserBuf` / `UserBufMut`
types that copy page-at-a-time through the direct map. We built these types
but the syscall handler refactoring had bugs. The types themselves
(`copy_from_user` / `copy_to_user` with per-page translation) are correct.
The handlers need careful porting — do it one syscall at a time with testing
between each.

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

## 6. Demand paging called from user_ptr translate()

When `virt_to_phys` returns None, `user_ptr::translate()` calls
`process::handle_page_fault()` directly. This works but is fragile — if
`handle_page_fault` ever needs locks that are held by the caller, it deadlocks.
The old approach (kernel dereferences user pointer, CPU faults, fault handler
maps page, instruction retries) was more robust.

**Fix:** Either (a) ensure all user pages are faulted in before syscall entry
(a "prefault" pass), or (b) accept the current approach but audit
`handle_page_fault` to ensure it never takes locks that syscall handlers hold.
Option (a) is cleaner but slower for large buffers.
