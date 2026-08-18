---
status: open
kind: defect
opened: 2026-08-13
---

# Contention has no owning instrument

`specs/testing-strategy.md` §1: "A class with no owning instrument is
recorded as unowned in `issues/`." §2's table names four instruments
and, for each, what it owns and what it is blind to. None of the four owns
contention — parallel suites contending for a lock, a lock storm, a
scheduler collapse under load. KVM CI (`ci.yml`'s `guest` shards) is one
guest per machine by construction, so there is never a second guest to
contend with; §2's own row says so, listing "contention" under KVM guest
shards' "blind to". The local suite is developer feedback and never a gate
(§6): a contention defect that surfaces only there merges anyway.

§2's table used to carry a row for exactly this class — "loaded dev host |
contention: parallel suites, lock storms, scheduler collapse | vendor
semantics; run-to-run comparability" — and that row is what found the
`ALONE: GREEN` verdict family and the load-coincident audio failures this
tree's history is full of (`issues/build/parallel-tests-red-under-other-suites.md`,
`issues/build/parallel-phase-starved-by-another-build.md`, the owner's
2026-08-04 ruling that a load-coincident audio failure is a real defect and
not noise). The row left the table when the strategy was rewritten to four
instruments; nothing replaced it, and the class it owned has carried no
instrument since.

This is the record §1 requires. It stays open until something owns the
class — a new instrument, or an existing one's scope widening to cover it.
