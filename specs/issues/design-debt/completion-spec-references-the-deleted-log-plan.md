---
status: open
kind: defect
opened: 2026-08-18
---

# `completion-architecture-spec.md` references the deleted log plan 42 times

`specs/log-architecture-spec.md` was a completed plan and was deleted. The
completion architecture was designed beside it and cites it throughout, so it
now carries the only set of dangling references left in `specs/`. Counted
2026-08-18: **14 by path** (`grep -c 'log-architecture-spec'`) and **28 as a
bare `log §N`** (`grep -c 'log §'`).

They are not all the same thing:

- Some are history that reads fine without the pointer — "L6 deletes
  `log_file.rs` whole" is a statement about a tree that exists.
- Some are load-bearing for that document's own plan: its C6 row says the
  kernel-thread machinery is already built and names the other document for what
  `klogd` is; its C0 row inherits a tree it describes by reference; its §23
  rejection 11 says the argument for a userland `logd` is in the other document
  rather than in itself, which after the deletion means the argument is nowhere.

The third kind is the one that costs something: an argument a spec delegates to
a document that is gone is an argument the next reader cannot check, and this
spec is still pending work rather than a record.

**Not fixed here, deliberately.** That document is a live plan with an open
branch, its C0 already owes a citation pass over the same subject, and 42 edits
to it from outside would collide with whoever holds it. Whoever opens C0 does
this in the merge commit, as that section already says.
