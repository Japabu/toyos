---
status: open
kind: defect
opened: 2026-08-15
---

# toyos-sched cites spec sections the behavior-only cut deleted

`25b2a04` cut the scheduler-core spec from 1,140 lines to 105 —
"behavior only" — deleting §10 (the harness sections) and §11 (the
stage ledger) among others. `toyos-sched/`'s comments still cite
§10.1, §10.2, §10.4 and §11's stage rows in many places, and none of
those sections exist: every such citation now points a reader at
nothing, which is exactly the shape the W9 sweep once fixed elsewhere
("the specs actively mislead agents").

Found during the negative-gates wave (PR #76), which fixed only the
one citation on its own path (`trace.rs`) and left the rest, correctly,
to its own entry. The fix is a sweep with a decision per citation:
inline the deleted fact into the module header at the site, which is
where a durable fact belongs, or delete a comment whose whole content
was the pointer. The tree at `25b2a04^` is the recovery source for what
each citation meant.
