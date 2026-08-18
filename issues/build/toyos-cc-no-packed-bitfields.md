---
status: open
kind: finding
opened: 2026-08-05
---

# `toyos-cc` does not implement packed bitfield layout, and says so

`__attribute__((packed))` on a struct with a bitfield member is refused —
`resolve_struct` in `toyos-cc/src/codegen/resolve.rs`, covered by
`toyos-cc/tests/attributes.rs`. Packed and unpacked bitfields are different
algorithms rather than one algorithm with a flag: gcc allocates a packed
bitfield's bits contiguously from the current bit position and lets a field
straddle what would have been a storage-unit boundary, where
`walk_struct_layout` picks a storage unit of the member's own type and starts a
new one whenever the next field would not fit. `codegen/bitfield.rs` loads and
stores through `clif_type(storage_ty)` at the field's byte offset, which a
straddling field has no single unit for.

`specs/plans/wlan-plan.md` §10 counts 635 `__packed` uses in the AX210 subset. However
many of those carry bitfields is how much of this W6 needs.
