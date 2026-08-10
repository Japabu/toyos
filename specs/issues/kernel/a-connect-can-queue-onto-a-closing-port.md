---
status: open
kind: defect
opened: 2026-08-10
---

# A client can queue onto a port that has just closed, and then never hears

`Connector::push` reads `closed` **outside** the queue lock and takes the lock
afterwards (`kernel/src/object/port.rs`); `Acceptor::on_zero_handles` stores
`closed` and *then* takes the queue. So:

```
CPU A (connect)                     CPU B (last acceptor handle drops)
read closed == false
                                    closed = true
                                    take the queue, drop it, wake acceptors
lock the queue, push_back
```

The `PendingConnection` that lands there is orphaned. No hook will run on it —
`Acceptor::on_zero_handles` has already run and runs once — so its `inbox` is
never `close_now`'d and the server-side pipe ends stay alive. The client holds a
`Connection` whose peer end exists, so its read blocks for ever and its write
succeeds into a ring nobody will read.

That is the one outcome `specs/capability-endowment-spec.md` §0 says cannot
happen: *"If a server exits without serving … the client's next write is
`SyscallError::Gone`. The bound on failure is a process lifetime and nothing
else."* Here the bound is nothing at all.

It is also how the cascade `specs/issues/kernel/ring0-jump-to-zero-under-port-polls.md`
§3 calls *"unreachable rather than absent"* becomes reachable: that section
argues the queue "is already empty by the time the last port reference can go",
which this interleaving falsifies.

## The fix

`closed` is a decision about the queue, so it belongs under the queue's lock —
one `Lock<PortQueue>` holding both the flag and the `VecDeque`, with `push`
checking and inserting in one acquisition and the hook setting and draining in
one. The `AtomicBool` buys a lock-free read on a path that takes the lock in the
next statement anyway.

## Instrument

TCG will not stage it: the window is a few instructions and `port_poll_churn` is
the nearest existing shape. A `kernel-loom` model of `PortShared`'s two paths is
the honest instrument — it is the only thing in the tree that checks an ordering,
and this is an ordering.
