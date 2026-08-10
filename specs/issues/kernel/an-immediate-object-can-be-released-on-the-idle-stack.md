---
status: open
kind: defect
opened: 2026-08-10
---

# A `deferred` object can own an `immediate` one, and the file flush is back

`kobject!`'s two classifications exist because of a measured defect
(`6d81a73`): `HandleEntry::drop` enqueued *every* object, so a killed process's
dirty file was flushed from `drain_zero_handles` in the idle loop, and the VFS
write path wrote through the guard page below the **16 KiB** per-CPU idle stack.
The fix made `File` an `immediate` row, so its destructor runs inline on the
dropping thread's 128 KiB stack.

Chunk 6 then put an arbitrary `HandleEntry` inside two `deferred` objects:

- `HandleQueue(Lock<Option<VecDeque<Vec<HandleEntry>>>>)`
  (`kernel/src/object/service.rs`) — in-flight handles of **any** kind.
- `ConnectionEnd::on_zero_handles` → `self.inbox.close_now()`, which takes the
  batches out and drops them on the drain's own stack.
- `Acceptor::on_zero_handles` (`kernel/src/object/port.rs`) does the same for
  every queued `PendingConnection`'s inbox.

`HandleEntry::drop` only enqueues if `defers_release()`, and `File` does not —
so `OpenFileState::drop` → `crate::vfs::lock()` → `flush_file` runs right there.
`drain_zero_handles` is called from `idle_loop` (`kernel/src/sched/driver.rs`),
whose `rsp` is `idle_stack_top()` and whose stack is `IDLE_STACK_SIZE = 16384`
(`kernel/src/arch/percpu.rs`).

A `File` handle carries `Rights::TRANSFER` (`kernel/src/object/ops.rs`), so
`SYS_HANDLE_SEND` accepts it. The shape: open and write a file, send its handle
over a connection, let the peer die without receiving. Its `ConnectionEnd`'s
count reaches zero, the hook is deferred, and the flush runs from the idle loop.

Nothing in the tree does this — the only two `handle_send` call sites in the
whole test estate send a `SharedMem` (`abuse_shared_grant`,
`shm_release_reclaims`) — which is why the suite is green.

## What is actually wrong

**The invariant "an `immediate` row's `Drop` never runs on the idle stack" is
not enforced anywhere**, and the macro cannot see it: `defers_release()` is a
property of the *outermost* type, and nothing forbids a `deferred` object from
owning an `immediate` one.

Two candidate fixes, and the second is the one to argue for:

1. Make `HandleQueue::close_now` enqueue each entry rather than drop it. Puts
   the batch back on the deferred queue where the hooks run with nothing held —
   but the entries whose rows are `immediate` still run their destructors on
   whichever stack drains.
2. Give the deferred drain a stack. Everything in `drain_zero_handles` is a
   release path, and a release path that can reach the VFS needs more than
   16 KiB. `IDLE_STACK_SIZE` is 16 KiB against 128 KiB for a task, and the
   drain is the only thing on the idle path that can go deep.

Same file, same measurement: `specs/issues/kernel/ring0-jump-to-zero-under-port-polls.md`
§5 asked this question of `SharedMem` and answered it, and wrote of `Connection`
only *"drops `HandleEntry`s and can enqueue further zero-handle work"* — which
is true for the `deferred` rows and not for the `immediate` ones.
