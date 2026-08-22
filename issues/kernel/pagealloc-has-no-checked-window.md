---
status: open
kind: finding
opened: 2026-08-22
---

# `PageAlloc` hands out a bare `*mut u8`, so the demand-paging fill is bounds-checked by argument

`kernel/src/process.rs`'s `PageAlloc` (contiguous 2 MiB physical pages from the
PMM, reached through the direct map) exposes `ptr() -> *mut u8` and `size()`,
and nothing else. `handle_page_fault` therefore fills a freshly allocated 2 MiB
frame through raw pointer arithmetic at two sites:

- `copy_nonoverlapping(page_buf.as_ptr(), page_ptr.add(page_offset), valid)` —
  the file-backed segment fill.
- `ri.apply_to_page(page_elf_offset, unsafe { page_ptr.add(offset as usize) })`
  — the relocation pass over the same frame.

Both are in bounds, and their `SAFETY:` comments (added by the
`undocumented_unsafe_blocks` sweep of the root files, 2026-08-22) derive it from
the loop conditions. Neither is *checked*.

## Why the sweep did not just add a window type

The obvious reduction is `PageAlloc::as_mut_slice(&mut self) -> &mut [u8]`,
which makes both sites safe and bounds-checked. It is also wrong: these exact
pages become a user mapping four statements later (`MappedPages` holds a
`PageAlloc` by value, so `&mut MappedPages` would reach the same accessor), and
a `&mut [u8]` over a page a process maps writable is the borrow
`kernel/src/user_ptr.rs`'s [`UserBytes`] header exists to refuse — `noalias`
over bytes another thread can change.

The right shape is the one `UserBytesMut` already has: a bounded window that
hands out no reference, with `write_at`/`ptr_at` doing the copy behind an
assert. `UserStack::write_at` (added by the same sweep, in the same file) is
that shape for the argv writes and is the precedent. What makes this a decision
rather than a mechanical change is that it wants to be *one* abstraction shared
by `PageAlloc`, `MappedPages`, `UserStack` and `mm::KernelSlice` — whose own
`from_raw` gap is
`issues/design-debt/kernelslice-from-raw-cannot-check-itself.md` — rather than a
fourth private one.
