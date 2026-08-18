---
status: open
kind: finding
opened: 2026-08-17
---

# `completion-architecture-spec.md` cites `log::console::pending()` as dead code still to delete; it is already gone

`specs/completion-architecture-spec.md` (§11 and §19) describes
`log::console::pending()` in `kernel/src/log/console.rs` as an *uncalled*
function — `serial::has_console() && DRAINED.any_pending()` — with zero callers
repo-wide, and says the ledger entry owed is "delete the function, not a
condition."

The function is not in the tree to delete:

```
$ grep -n pending kernel/src/log/console.rs
214:fn discard_pending() {
...(no `fn pending`, no `console::pending`)
```

Either it was deleted independently of this branch (the way `futex_wake`'s
generation protocol was, which the same document already tracks as "struck
from this list: it is already gone"), or the citation was wrong about the
function's name or location even before this document's line numbers were
last checked. Either way, the deliverable the document assigns —
delete-the-dead-function — has nothing left to point at.

Filed as a finding rather than a defect because nothing misbehaves; the
document's own accounting of what C13 owes is what needs reconciling.

Found 2026-08-17 during a citation-accuracy pass over
`specs/completion-architecture-spec.md`; verified at the tree's tip that day.
