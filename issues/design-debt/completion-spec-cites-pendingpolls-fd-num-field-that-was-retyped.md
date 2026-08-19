---
status: open
kind: finding
opened: 2026-08-17
---

# `completion-architecture-spec.md` §19 says `PendingPoll` still carries `fd_num: u32`; the field is now `handle: RawHandle`

`completion-architecture-spec.md` §19 records `kernel/src/listener.rs`
and `wake_poll_waiters` as already deleted by the endowment branch, and adds:
"`PendingPoll` survives … and still carries its own `fd_num: u32` field, which
no spec ever named."

`PendingPoll`'s fields in the current tree:

```
kernel/src/io_uring.rs:192  struct PendingPoll {
    user_data: u64,
    handle: RawHandle,   // "the handle the poll was submitted against"
    flags: PollFlags,
    read_source: Option<Source>,
    write_source: Option<Source>,
}
```

There is no `fd_num` field. The struct was retyped from a bare fd number to a
capability handle at some point after this section was last checked against
the tree — consistent with the rest of the endowment branch's handle-based
model, but the specific claim in §19 is now false rather than merely
line-rotted: the field's *name and type* changed, not just its position.

Filed as a finding rather than a defect because nothing misbehaves; it is
`completion-architecture-spec.md`'s own accounting of "what C3 owns"
that needs reconciling with the field's current shape.

Found 2026-08-17 during a citation-accuracy pass over
`completion-architecture-spec.md`; verified at the tree's tip that day.
