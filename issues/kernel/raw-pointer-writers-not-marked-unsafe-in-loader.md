---
status: open
kind: defect
opened: 2026-08-20
---

# Two loader functions write through a raw pointer without being `unsafe fn`

Writing `undocumented_unsafe_blocks`' required safety comments on `elf/` and
`loader/` turned up the same pattern twice: a function that is *not* `unsafe
fn` takes a bare raw pointer and writes through it inside its own `unsafe {
}` block, so the requirement that the pointer be valid for the range written
is real but is neither type-enforced nor documented on the function itself.

- `elf::read_backing_into` (`elf/mod.rs`, `pub(crate) fn ... dst: *mut u8,
  len: usize) -> BlockResult`) — the `unsafe { copy_nonoverlapping(...,
  dst.add(buf_off), chunk) }` in its read loop. Three call sites, each
  correct today, checked by hand for this pass:
  - `elf::mod::load_shared_lib` (`elf/mod.rs:404` in the current numbering)
    passes a `KernelSlice::subslice`'s own `(base, size)` — bounds-checked by
    `subslice` itself.
  - `loader::spawn`'s TLS read (`loader/mod.rs:578`) allocates its buffer
    sized `tls.memsz` and reads `tls.filesz` bytes — sound only because
    `toyos_elf::Layout::parse` already refuses `filesz > memsz` for `PT_TLS`
    (`toyos-elf/src/layout.rs:176`, `Error::FileszAboveMemsz`).
  - `loader::symbols::read_backtrace_table` (`loader/symbols.rs:149-155`)
    partitions one `PageAlloc` of exactly `syms.size + strs.size` bytes into
    two back-to-back writes of `syms.size` and `strs.size`.

- `elf::index::RelocationIndex::apply_to_page` (`elf/index.rs`, `pub fn
  apply_to_page(&self, page_offset: u64, page_ptr: *mut u8) -> usize`) — two
  `unsafe { write_unaligned(page_ptr.add(within_page) as *mut _, value) }`
  sites, each already bounded against a 4096-byte page by `within_page + {8,4}
  <= 4096`. One call site (`process.rs:1688`, the demand-paging fault
  handler): `page_ptr.add(offset as usize)` inside a loop bounded by `offset
  < page_2m` stepping by exactly 4096, so every call stays inside the 2 MiB
  buffer `page_ptr` is documented (by the caller's own local reasoning, not
  by `apply_to_page`'s signature) to point at.

Every call site is correct today, but none of that is enforced by either
function's own signature or body — neither checks its length/offset argument
against anything the pointer itself proves, and neither requires `unsafe` at
the call site. A future caller with a buffer one iteration short, or a page
offset computed one field wrong, compiles clean and corrupts memory silently,
with no `unsafe` keyword anywhere near the actual mistake. CLAUDE.md's own
bar — "prefer compile-time safety over runtime checks over tests" — is
exactly what both functions' current shape does not do: the requirement is
representable (`unsafe fn` plus a `# Safety` doc, matching every other
raw-pointer API in `mm::region::KernelSlice`) and currently is not
represented that way for either.

Not fixed here — this pass's job was documenting existing `unsafe` blocks, not
changing function signatures, and both functions have real call sites across
`elf/`, `loader/` and `process.rs` that would each need to grow an `unsafe` at
the call site (or take a `KernelSlice` instead of a bare `(*mut u8, usize)` —
closer to this module's own idiom everywhere else, and self-checking).
