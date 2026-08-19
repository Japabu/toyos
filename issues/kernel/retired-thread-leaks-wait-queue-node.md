---
status: open
kind: defect
opened: 2026-07-30
---

# A thread retired while parked leaves its node in the wait queue forever

`Msg::Retire` reaps a `BlockedTask` on its home CPU; the `Registration` that
would have dequeued it lives on the dead thread's own stack and is never dropped
(the kernel does not unwind), so the queue keeps an `Arc<TaskShared>` for a task
that is `Dead`. A leak, not a correctness hole — `claim_wake` on a dead task
returns `Claim::Lost` and `wake_one` moves to the next waiter, which is exactly
what the wake path's retry arm exists for — but the list grows across process
kills and a `wake_all` walks the corpses. The fix belongs with the intrusive
`wait_node` the core still owes (`waitq.rs` holds waiters in a `VecDeque`
instead; see its module note): with an embedded node, `reap` could unlink it.
