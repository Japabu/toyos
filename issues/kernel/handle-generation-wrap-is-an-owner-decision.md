---
status: owner
kind: question
opened: 2026-08-20
---

# Handle-generation wrap is an owner decision, not a missing stress test

From the external review of 2026-08-20: if handles carry a finite generation
counter, what happens when it wraps is an architectural security decision. An
ancient stale handle that eventually becomes valid again is a bounded ABA
problem — a use-after-free of authority, however improbable. The policy
options, one of which must be chosen and documented at the site
(`toyos-abi/src/handle.rs` and the kernel's table):

1. **Permanently retire a slot when its generation exhausts** — the table
   shrinks by one slot per exhaustion; simplest sound answer.
2. **Widen the generation field** — moves the horizon, does not remove it.
3. **Randomized token/cookie design** — removes the sequential ABA at the
   cost of comparing wider values.
4. **Explicitly accept wraparound under a stated threat model** — legal only
   if written down with the numbers.

**Orchestrator's recommendation: option 1.** It matches the tree's own
retirement instinct (a deleted syscall's number is never reused), it is
provable by a near-exhaustion test staged with an actuator rather than
brute-force lifecycle billions, and a machine that has genuinely exhausted a
slot's generations has earned a smaller table.

Whichever is chosen: the property is tested NEAR exhaustion directly — an
actuator that sets a slot's generation to the last value and asserts the
policy — never by running the counter up for real.
