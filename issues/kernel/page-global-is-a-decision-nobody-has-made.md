---
status: owner
kind: question
opened: 2026-08-19
---

# There is no `PAGE_GLOBAL` anywhere, and that is a decision nobody has made

`CR4.PGE` is absent from `kernel/src/arch/control_regs.rs`'s declaration and no
`PAGE_GLOBAL` exists in `kernel/src/mm/`. With PCID active, every address space
therefore caches a private copy of the kernel's direct map, and every
`flush_tlb_all` — which is `INVPCID` all-context — throws away the kernel's own
translations on every CPU along with the process's.

This is not an oversight to fix silently. 2020+ hardware is not Meltdown-
affected and this kernel has no KPTI, so global kernel pages are available.
`control_regs.rs`'s own doctrine — every bit of both registers is decided in one
place, and the bits left out are as much of the declaration as the bits in —
means `PGE`'s absence should be *stated* whichever way it goes.

**What the answer would have to revisit.** `kernel/src/mm/paging.rs` now derives
every invalidation from the entry a write replaced and discharges it with
`INVPCID` type 0 or `INVLPG`. Neither touches a global translation, which is
sound only because no entry in this kernel is global; the module header says so
in as many words, and a `PGE` that goes in has to answer for every `discharge`
there and for `guard_4k`'s local full flush.

This file was `tlb-invalidation-is-chosen-not-derived`, whose other half — an
invalidation chosen by each caller, an unconditional `invlpg` over a not-present
entry, a duplicate one on the demand-paging path, and one aimed at the parent's
PCID while writing a child's tables — is resolved. `git log --follow` on this
file carries the evidence; `Owed` in `mm/paging.rs` is what it became.
