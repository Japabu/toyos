---
status: open
kind: defect
opened: 2026-08-10
---

# `SYS_HANDLE_SEND` takes the handles out and then may refuse

`sys_handle_send` (`kernel/src/arch/syscall.rs`) verifies every handle under one
hold of the sender's table, `remove`s them all, and *then* calls `conn.send`:

```rust
let mut batch = Vec::with_capacity(count);
for h in wanted {
    batch.push(data.handles.remove(*h).expect("verified under this same hold"));
}
...
match conn.send(batch) {
    Ok(()) => 0,
    Err(e) => e.to_u64(),
}
```

`HandleQueue::push` refuses on two conditions (`kernel/src/object/service.rs`):
the peer's end has gone (`Gone`), and the queue already holds
`MAX_QUEUED_BATCHES = 16` (`ResourceExhausted`). On the `Err` arm the batch is
dropped, so the handles are **destroyed** — their objects' counts fall, their
slots are gone and the generations bumped.

The caller is told `ResourceExhausted`, an ordinary backpressure word. Its
handles are not "still here to try again": they no longer exist. A caller that
retries with the same numbers gets `Stale`, which ends it (exit 139). The
comment at the call site says the two facts are ones *"a caller reads as 'the
handles did not go'"* — they did go, and then they were burned.

`Gone` is arguably fine (nobody can ever use them again). `ResourceExhausted` is
not: it is exactly the condition a slow or hostile client produces, and the
server that hits it is the one that loses a capability.

## The mirror, and why only half of it is fixed

`SYS_HANDLE_RECV` had the same shape from the other side — `pop_bounded` popped
the batch and the table-room check came after — and that one is fixed on this
branch: the width is measured with `ConnectionEnd::peek_width` and both refusals
leave the batch queued. The send side cannot be fixed the same way, because
between a "has the queue room" check and the push the *peer* can close its end.

## The fix

Two-phase, under the sender's table lock which the whole call already holds:
take the entries without bumping their slots' generations, attempt the push, and
on refusal put them back at the same slots. `HandleTable::remove`'s generation
bump is what makes a put-back at the same handle number unrepresentable today,
so the change is a `take_for_transfer`/`restore` pair beside it — the slot stays
empty at its current generation for the length of one call, which nothing else
can observe because the lock is held.

`abuse_fd_table` is the family; the arm is a peer that never receives, a sender
that fills sixteen batches, and the assertion that the seventeenth send's
handles still resolve afterwards.
