---
status: open
kind: defect
opened: 2026-08-16
---

# mio's ToyOS selector deregisters a token but not the kernel's poll on it

`src/sys/toyos/selector.rs` in the mio fork (currently pinned at `e8068c2`,
`userland/Cargo.lock`) keeps its own registration list rather than asking the
kernel to track interest:

```rust
pub fn deregister_fd(&self, fd: RawHandle) -> io::Result<()> {
    let mut inner = self.inner.lock().unwrap();
    inner.registrations.retain(|r| r.0 != fd);
    Ok(())
}
```

This removes the entry from `SelectorInner::registrations`, so `select()`
(lines 169–214) stops re-arming a `POLL_ADD` for it on future calls — the
re-arm loop at 172–185 only iterates what is still in the list. But a
`POLL_ADD` already submitted for that fd in an *earlier* `select()` is not
touched: nothing here cancels it, and nothing waits for it to land. If the
kernel completes that stale poll after `deregister_fd` returns, the CQE still
carries the old token, and the next `select()`'s drain loop (198–211) does not
check the token against the current registration list before turning it into
an `Event` — it matches purely on `cqe.user_data`. The caller (tokio's
reactor, or anything else built on mio's `Registry`) gets one readiness
notification for a resource it was promised was gone.

## Why there is nothing to cancel with

There used to be an ABI op for this. `toyos-abi/src/io_uring.rs` op code 2 was
`IORING_OP_POLL_REMOVE`, retired in PR #89 (`c41b831`,
"abi: four names retired, and the number each one held") as caller-less. The
retirement's own reasoning is recorded at the site:

```
// Op code 2 unused (formerly IORING_OP_POLL_REMOVE). It had no submitter
// anywhere either — and the selector that would have been its caller cancels
// nothing: mio's ToyOS selector keeps its own registration list, re-arms every
// registration on each `select`, and deregisters by dropping the entry. A poll
// this kernel takes is one-shot, consumed by the completion it posts, so the
// interest a remove would withdraw is gone before there is anything to name.
```

The last sentence is the part this entry disagrees with. "One-shot, consumed
by the completion it posts" is true of the poll itself, but it says nothing
about *when* that completion is consumed relative to `deregister_fd` — and the
selector's own code shows the two are not synchronized. The retirement
correctly observed that mio never calls a remove op; it did not establish that
nothing needs one. Those are different claims, and PR #89 only re-verified the
first (grepped the estate for a submitter — found none, which is true and
remains true) rather than asking whether `deregister_fd`'s behavior was itself
correct.

## Why it matters for pipeline 2

The completion track makes every kernel wait a cancellable completion, answered
by `Cancelled` rather than by discarding the stack; the mechanism-consolidation
audit (`specs/assessments/2026-08-15-mechanism-consolidation-audit.md`, Wave B)
calls the userland-facing half of that "pipeline 2" and names its cancellation
rewrite the highest-risk piece of the whole track. mio's selector is exactly
the kind of consumer that rewrite has to get right: a `deregister` that cannot
promise "no event after this point" is the userland mirror of the kernel-side
bug pipeline 2 exists to remove. Re-adding a cancel op (or otherwise making
`deregister_fd` synchronous with the kernel's pending state — draining the SQ/CQ
for that fd, or having the kernel answer a targeted query) is in scope for
whoever designs that half; retiring `IORING_OP_POLL_REMOVE` a second time
without addressing this would repeat the same reasoning gap.

Filed rather than fixed: this is a fork (mio, not this repository) and a
design question (what the cancel primitive should look like once pipeline 2
exists), not a local bug fix.
