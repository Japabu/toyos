---
status: open
kind: defect
opened: 2026-08-13
---

# Contention has no owning instrument

Every defect class has exactly one owning instrument, and a class with no
owning instrument is recorded as unowned here. Four instruments carry the whole
estate — host suites, KVM guest shards, the TCG shard, and metal — and none of
them owns contention: parallel suites contending for a lock, a lock storm, a
scheduler collapse under load. KVM CI (`ci.yml`'s `guest` shards) is one
guest per machine by construction, so there is never a second guest to
contend with, and the KVM row says so itself, listing "contention" under its
own "blind to". The local suite is developer feedback and never a gate: a
contention defect that surfaces only there merges anyway.

The table used to carry a row for exactly this class — "loaded dev host |
contention: parallel suites, lock storms, scheduler collapse | vendor
semantics; run-to-run comparability" — and that row is what found the
`ALONE: GREEN` verdict family and the load-coincident audio failures this
tree's history is full of (`issues/build/parallel-tests-red-under-other-suites.md`,
`issues/build/parallel-phase-starved-by-another-build.md`, the owner's
2026-08-04 ruling that a load-coincident audio failure is a real defect and
not noise). The row left when the table was rewritten to four instruments;
nothing replaced it, and the class it owned has carried no instrument since.

This is the record that rule requires. It stays open until something owns the
class — a new instrument, or an existing one's scope widening to cover it.
