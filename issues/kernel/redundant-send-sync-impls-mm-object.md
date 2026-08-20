---
status: open
kind: defect
opened: 2026-08-20
---

# `unsafe impl Send`/`Sync` for `Pages` and `AddressSpace` look vestigial

Writing `undocumented_unsafe_blocks`' required safety comments on
`kernel/src/object/shm.rs` and `kernel/src/mm/paging.rs` turned up two hand
-written auto-trait impls that a `cargo check` says are unnecessary:

- `object::shm::Pages(Vec<pmm::PhysPage>)` — `unsafe impl Send for Pages {}`
  and `unsafe impl Sync for Pages {}` (`shm.rs:52-53` in the current line
  numbers).
- `mm::paging::AddressSpace` — the same pair, over a struct of
  `Box<PageTablePage>`, `Vec<Box<PageTablePage>>`, `HashMap<u64,
  pmm::PhysPage>`, `BTreeMap<UserAddr, vma::Region>` and `u16`
  (`paging.rs:508-510` in the current line numbers, immediately after the
  struct).

In both cases every field is, transitively, a type that is already
`Send`/`Sync` without help: `pmm::PhysPage` is `{phys: u64, category: u8}`
with no manual impl and no raw pointer; `PageTablePage` is `[u64; 512]`;
`vma::RegionKind::FileBacked` holds `Arc<dyn FileBacking>`, and
`FileBacking: Send + Sync` is a supertrait, so the trait object inherits
both automatically. Deleting all four `unsafe impl` lines and running `cargo
check --target x86_64-unknown-none` from `kernel/` compiles clean either way
(verified 2026-08-20, both sites, independently) — nothing downstream
requires them.

**Not fixed here.** Removing dead code is a real change to a security
-relevant module (`AddressSpace` is the page-table owner) and this pass's
job was documentation, not editing the invariant it documents. Two are
recorded together because they are the same mistake, not because either
depends on the other, and because a third or fourth instance elsewhere in
the kernel is plausible — this pass did not go looking for more, since
`undocumented_unsafe_blocks` only turns up an `unsafe impl` that still lacks
a comment, and older, already-commented ones would not have surfaced this
way.

The actual hazard is forward-looking, not present-tense: a hand-written
`unsafe impl Send`/`Sync` stops the compiler from re-deriving the bound on
every change. If a later field addition to `PhysPage` or `AddressSpace`
broke the auto-derive — a raw pointer, an `Rc`, a `Cell` — these impls would
silently keep the type compiling as `Send`/`Sync` with no new review, which
is a correctness hazard specifically because both types are built out of
raw physical addresses and page-table state. Deleting the four lines (so the
auto-derive becomes the enforcement) is the fix; confirming there is no
third instance elsewhere in the kernel is the other half.
