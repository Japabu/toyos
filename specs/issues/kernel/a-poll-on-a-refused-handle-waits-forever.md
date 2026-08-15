---
status: open
kind: defect
opened: 2026-08-15
---

# A `POLL_ADD` on a refused handle registers nothing and waits forever

`process_poll_add` resolves the handle through one `let … else` that swallows
three different refusals and answers all of them the same way:

```rust
// kernel/src/io_uring.rs:549
let Ok(object) = data.handles.get_ref(handle, Rights::WAIT) else {
    return (false, None, None);
};
```

`get_ref` fails for a handle that was never valid (`BadHandle`), one whose object
is gone (`Stale`), and one held without `Rights::WAIT`. The comment above the
block covers only the third: *"A handle without `WAIT` is not watchable, and
answers as if it were not there."* That is a deliberate policy and is not what
this entry is about.

The first two are not. `specs/capability-endowment-spec.md` and
`kernel/src/object/handle.rs:57-69` state one rule for them — `refuse_as_error`
sends `Rights` to `PermissionDenied`, `TableFull` to `ResourceExhausted`, and
everything else to `process::handle_fault`, because naming a handle you do not
hold is a bug in the caller and the kernel ends it.
`rg -n 'handle_fault' kernel/src` returns exactly two lines, the definition and
that one call, so there is otherwise no second way to answer this question. This
site is a third answer the doctrine does not have.

## The consequence is worse than a wrong error code

Because the `else` returns `(false, None, None)`, `ready` is false and both
sources are `None`. The registration loop then iterates nothing:

```rust
// kernel/src/io_uring.rs:579
for src in [&read_source, &write_source].into_iter().flatten() {
    src.add_watcher(ring_id);
}
```

and a `PendingPoll` with both `read_source` and `write_source` set to `None` is
pushed onto `instance.pending_polls`. Nothing can ever complete it: no watcher
list holds the ring, so no event site will reach it, and `is_ready` is never
consulted again for a source that does not exist. The submitter gets no CQE, no
error and no kill — it blocks until an unrelated wake, and its own recheck finds
nothing.

So a program that polls a handle it already closed does not learn it made a
mistake; it goes quiet. That is the failure mode CLAUDE.md's *"the unimplemented
dies loudly"* exists to prevent, arrived at from the other side.

## Relationship to the existing entry

`specs/issues/kernel/io-uring-source-half-a-wake-pair.md` describes the same
*symptom* — a `POLL_ADD` that never completes — from a different cause: a source
that is wired but lost one half of its wake pair at the event site. This one is a
poll that was never wired at all, because the handle was refused and the refusal
was discarded. Both are fixed by the completion architecture's single `post()`,
where a poll cannot exist without the registration that completes it; neither is
fixed by the other.

Three sibling bypasses of the same shape were found in the same audit and are
noted here rather than filed separately, because they share one repair: `IORING_OP_ACCEPT`
answering `-InvalidArgument` in a CQE for a bad, stale or wrong-typed handle
(`kernel/src/io_uring.rs:639-650`) where `SYS_ACCEPT` correctly reaches
`handle_fault`; `SYS_DEVICE_REG` answering `NotFound` for an unheld handle
(`kernel/src/arch/syscall.rs:985-992`) where its sibling `holds_claim` does it
correctly; and `kernel/src/loader/start.rs:284`'s `let Ok(rights) = … else { continue }`,
which is argued at the site but is not one of the exceptions
`specs/capability-endowment-spec.md` §1.2 names.

## What a fix has to decide

Separating the arms is the easy half. The hard half is that the rights case and
the bad-handle case want different answers *at a submission point that has no
error channel until a CQE exists* — which is why they were folded. Deciding
whether a refused `POLL_ADD` posts a CQE carrying the refusal or kills the caller
outright is the question, and §1.2 of the endowment spec is where the answer
belongs once it is made.
