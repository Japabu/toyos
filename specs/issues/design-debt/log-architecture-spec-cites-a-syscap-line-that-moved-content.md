---
status: open
kind: finding
opened: 2026-08-17
---

# `log-architecture-spec.md` cites `capability-endowment-spec.md:2682` for a claim that spec no longer makes

`specs/log-architecture-spec.md` §0.0 says: *"113 is reserved for
`SYS_PORT_REARM`"*, citing `capability-endowment-spec.md:2682`.

`specs/capability-endowment-spec.md` is 226 lines today. Line 2682 does not
exist — the cited document has been rewritten down to a fraction of its size
since this citation was written. What it says now, at its own line 126, is
narrower than the claim: *"A retired syscall number is never reused. Number
113 is reserved and carries nothing"* — it no longer names `SYS_PORT_REARM`
at all.

The underlying fact is not false: `toyos-abi/src/syscall.rs`'s reservation
comment still reads *"Number 113 is **reserved, not free**: it is held for
`SYS_PORT_REARM`"*. So `capability-endowment-spec.md` is simply the wrong
citation for this specific attribution now — the ABI source is the accurate
one.

Per the citations-only scope of the pass that found this (converting
`file.rs:NNN` citations to file-plus-symbol form, mirroring PR #110), the
line number was dropped but the citation target was **not** repointed to
`toyos-abi/src/syscall.rs`, and the claim's wording was not touched — doing
either would have been fixing the claim rather than reporting the drift.
`specs/log-architecture-spec.md` now cites bare `capability-endowment-spec.md`
for this row, which is honest about what changed (a line number) but not
about what is owed (the citation target itself).

Found 2026-08-17 during that pass; verified at the tree's tip that day.
