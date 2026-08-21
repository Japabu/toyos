---
status: open
kind: defect
opened: 2026-08-19
---

# `process::futex_wait` documents a timeout return it cannot produce

`kernel/src/process.rs`'s `futex_wait` says:

> Returns 0 if woken normally, 1 if timed out, an error if `addr` names no word
> this process may have.

No path returns 1. It calls `scheduler::futex_wait`, which is `-> bool` and
answers `false` only for the value-mismatch case — the timeout and the normal
wake are both `block_on(ticket, deadline)` returning, after which it answers
`true`. So a caller that passes a timeout cannot tell a timeout from a wake, and
the one number that would have told it is documented but never produced.

Found while clearing default clippy's `if_same_then_else` at that site
(`0 // blocked and woken` and `0 // value mismatch, returned immediately` were
two arms of one `if`). The lint is now clear — the branch is gone and one
comment says both outcomes are the same answer — but that fix was deliberately
neutral, and the mismatch between the doc and the code is what is left.

Two things could be true, and which one decides the fix:

- **The doc is stale.** Userland re-checks the futex word after every wake, so a
  timeout that reads as a wake costs one extra check and nothing else. Then the
  fix is deleting the sentence and the returns stay `0`.
- **The caller needs it.** `SYS_FUTEX_WAIT`'s return reaches userland, and a
  `futex_wait` with a timeout that cannot report the timeout is a primitive
  nobody can build a bounded wait on. Then `scheduler::futex_wait` has to
  distinguish the two, which means `block_on` has to say which of the two woke
  it.

Nothing measured which. The syscall's userland callers are the evidence to
gather first.
