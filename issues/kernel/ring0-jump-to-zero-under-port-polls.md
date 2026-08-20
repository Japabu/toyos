---
status: open
kind: finding
opened: 2026-08-09
---

# A Ring 0 instruction fetch at address 0, once, in a process polling two ports

Seen exactly once, on `wt/toyos-endow` during chunk 5's second full suite run
(2026-08-09), and **not reproduced**: the same test alone was green, and two
later full runs of the same tree were green. Filed because a Ring 0 jump to
address 0 is the kernel crashing from userland, which is the one thing it may
never do — so a single sighting is worth the file even without a repro.

## What the machine said

The shared `tests/testcases` boot, during `rs::sched_stress`, which had just
been rewritten from the deleted `SYS_LISTEN`/`SYS_CONNECT` onto ports:

```
[kernel 7.059 cpu0 tid=4] #PF UNHANDLED: cr2=0x0 rip=0x0 err=0x10 user=false tid=Some(Tid(4))
[kernel 7.059 cpu0 tid=4] SEGFAULT tid=4: execute unmapped address at 0x0
[kernel 7.059 cpu0 tid=4]     rbp=0x0000000000000000  rsp=0xffff800008062c50
[kernel 7.059 cpu0 tid=4]     cs=0x0008  ss=0x0010  rflags=0x0000000000010002
[kernel 7.059 cpu0 tid=4]   Backtrace:
[kernel 7.059 cpu0 tid=4]   Stack (from RSP):  … eight quadwords, all zero …
[kernel 7.059 cpu0 tid=4] poison_tid: cpu 0 slot still held 82:0 — its waiter is stranded
```

Three things in that are worth more than the address:

- **`cs=0x0008`, `user=false`** — the kernel, not the process, executed at 0.
- **The whole visible stack is zero and `rbp` is zero**, so this is not a call
  through one bad pointer with a live frame under it.
- **`poison_tid` says pid 82 tid 0 was *already* poisoned and never reaped**, so
  this was the *second* recovery for that process. The first fault's report is
  in the same log with `tid=0`.

pid 82 is `sched_stress` itself: it is the only test binary in that boot with
several threads. QEMU disconnected immediately after, taking 140 collateral reds
with it — the boot, not the test.

## What was running

`sched_stress`'s rewritten first case arms an `io_uring` `POLL_IN` on an
`Acceptor` handle from each of two threads, lets one time out, and **closes the
acceptor handle while its `PendingPoll` is still armed**, then drops the ring.
`Source::Port(Arc<PortShared>)` and `WatcherGuard` are chunk 3's code and had no
in-tree caller before this. The lock order there is `IO_URINGS` then
`PortShared::io_uring_watchers` on the arm path and the same way round on the
drop path, so it is not an obvious inversion — but nothing has audited the case
where the last `Acceptor` handle goes while a poll references the port.

---

## The chunk 6 audit, 2026-08-10 — what it proved, what it fixed, what is open

Chunk 6 is the chunk that touches port and connection lifetime, so this was
settled there as far as it can be settled. **It is not closed.** Three green
runs and fifteen more below are not evidence about a class that hides.

### 1. The register state names the mechanism, and it is not a corrupted pointer

`alloc_kernel_stack` (`kernel/src/loader/start.rs`) lays out the frame
`context_switch` restores: `rbp` at slot 5, `RFLAGS` at slot 6, the return
address at slot 7. The reported state is that frame **read back as all zeros**:

| reported | what an all-zero frame gives |
|---|---|
| `rflags=0x10002` | `popfq` of `0` yields `0x2` — bit 1 is reserved-always-set — and a fault sets `RF` (`0x10000`) in the saved word |
| `rbp=0` | slot 5 |
| `rip=0` | `ret` to slot 7 |
| eight quadwords from RSP all zero | the rest of the page |

`OwnedAlloc::new` allocates with `alloc_zeroed`, so a kernel stack that went
back to the heap and came out again is exactly zeros. **So this is not a jump
through a corrupted function pointer: it is `context_switch` resuming a task
whose saved kernel stack had been freed and reissued under it.** Nothing in the
port or io_uring code frees a kernel stack. What does is
`scheduler::reap_poisoned` → `process::zombify_poisoned`, on the recovery path
the `poison_tid` line is from.

### 2. `poison_tid` drops a thread on the floor, and the log says it did

`POISONED` is **one `AtomicU64` per CPU** and `poison_tid` `swap`s into it. A
second fault on the same CPU before the idle loop has reaped the first
overwrites it: the first thread is never zombified, its joiner is never woken,
and nothing else ever looks at it again. The line in the report is that
happening. So the sequence is *first* fault on 82:0 — whose cause is not in this
file and is the thing still wanted — and the `rip=0` on 82:4 downstream of the
recovery from it.

That makes the single-slot poison set a defect in its own right, filed
separately as
`issues/kernel/poison-set-holds-one-thread-per-cpu.md`.

### 3. Port lifetime: what holds `Arc<PortShared>`, and what drops it

Audited in code, not assumed.

- A `PendingPoll` holds the port **by `Arc`**, in `read_source`/`write_source`
  (`Source::Port(Arc<PortShared>)`). The watch holds what it watches, so **a
  watcher cannot outlive the port**, and the watch list is a field *of* the
  thing being kept alive — so "the list is walked after the object's last handle
  went" is not expressible either.
- The `Arc` is dropped by exactly five things, all of which now go through one
  function (§4): the poll completing, being replaced by a re-arm on the same
  handle, `POLL_REMOVE`, `cancel_by_source`'s cancellation, and the ring's own
  teardown.
- `Acceptor::on_zero_handles` sets `closed` and empties the queue. It runs
  *before* the `Arc<Acceptor>` in the deferred batch drops, so the queue is
  already empty by the time the last port reference can go — which is what keeps
  the cascade `PortShared` → `PendingConnection` → `PipeReader` → `PIPES` from
  ever running under `IO_URINGS`. That cascade would be a self-deadlock, because
  `close_read` re-enters `complete_pending_for_event` when the pipe still has a
  writer and a watcher. **It is unreachable rather than absent.**

  **That paragraph was false when it was written, and this is the correction.**
  It rested on "the queue is already empty", and `closed` was an `AtomicBool`
  read *outside* the queue's lock: a connect could read it false, the hook could
  then set it and drain, and the connect's `push_back` landed in a queue whose
  one hook had already run. So a connection could be queued onto a closing port,
  the cascade was reachable rather than unreachable, and the connection itself
  was orphaned — nothing closed its inbox, so the client's read blocked for ever,
  against the promise that the bound on failure is a process lifetime. Filed as
  `a-connect-can-queue-onto-a-closing-port` and fixed on the same branch: the
  flag now lives *inside* the queue's lock (`PortQueue`), so `push` asks and
  inserts in one acquisition and the hook closes and drains in one. The argument
  above holds again, and it now rests on a lock rather than on an ordering
  nothing states.

### 4. Two defects found by the audit, both fixed in chunk 6

- **The watcher list is a set of rings with no count, and `WatcherGuard`
  unregistered unconditionally.** Two polls of one ring on one source — a
  `dup`ped acceptor polled through both handles — meant the first to be removed
  took the ring out of the source's watcher list while the second was still
  armed. No wake ever reaches it again and nothing is visible: the poll sits in
  `pending_polls` for ever. The guard could not have been right, because whether
  a registration is still owed is a property of the *ring* and not of the poll.
  `WatcherGuard` is deleted and `io_uring::take_poll` is the one removal path.
- **`Acceptor::on_zero_handles` neither woke `SYS_ACCEPT` nor let it leave.** A
  thread parked in `accept` waits on `has_pending()`; the hook set `closed` and
  dropped the queue, making that condition permanently false, and posted no
  wake. A process whose second thread closed the last acceptor handle parked for
  ever. The hook wakes the acceptor queue now and `sys_accept` answers
  `SyscallError::Gone` on `closed`.

Neither is the fault above. Both are the class it was filed under.

### 5. `deferred` versus `immediate`, asked of every object chunk 6 touches

- `SharedMem` is **deferred**, and must be: its hook unmaps from every process
  and shoots down, which cannot run under the `ProcessData` lock `close_all`
  holds. The predecessor's hazard — a deep subsystem call landing on the 16 KiB
  idle stack — does not apply: the hook is `free_and_unmap` per mapping and one
  `Unmapped` drop, with no filesystem, no device and no allocation under it.
- `Acceptor` stays deferred; its hook drops pipe ends, which takes `PIPES`.
- `Connection` stays deferred and now also closes its inbound handle queue,
  which drops `HandleEntry`s and can enqueue further zero-handle work — a queue
  iteration, which `drain_zero_handles` already loops for.

  **True for the entries whose rows are `deferred`, and false for the rest**,
  which is the half this bullet missed. A `HandleEntry` for an `immediate` row
  enqueues nothing: its destructor runs inline, wherever the drain is running.
  So a `File` sent over a connection whose peer died reached `vfs::flush_file`
  from the drain, and the idle loop is one of the drain's three sites — the
  16 KiB idle stack, through the guard page, which is exactly the defect
  `6d81a73` had closed per object and which returned through a container. Filed
  as `an-immediate-object-can-be-released-on-the-idle-stack` and fixed on the
  same branch by giving the drain a stack: `IDLE_STACK_SIZE` is
  `KERNEL_STACK_SIZE` now, so there is one stack size for every context Rust
  kernel code runs on and the question this bullet was asking has one answer.
- `Connector`, `Namespace`, `File`, `Console`, `SysCap` are unchanged.

### 6. The instrument, and what it did not find

`tests/toyos-rust-tests/src/bin/port_poll_churn.rs` is the cheap instrument this
file asked for: two threads, 250 rounds each of arm-poll → close-acceptor →
drop-ring and the reverse order, with and without an unaccepted connection
queued behind the acceptor, plus 32 rounds of two rings on one port with one of
two acceptor handles closing under the other's live registration.

**Fifteen full runs, all green, no fault** (2026-08-10, this host, before the
chunk 6 changes). That is fifteen samples of a thing seen once in a suite, so it
says nothing except that the window is not trivially reachable. The binary stays
in the suite.

---

## 2026-08-20: the class this was filed under has been diagnosed, and this
sighting is not settled by it

Four later sightings of a Ring 0 instruction fetch at a tiny address were one
defect and it is fixed: a CPU could hand a thief the task **whose context it was
still standing on**, because a scheduler pass ends before the context switch it
decided on has run. `SchedPass::answer_steal_requests` in
`toyos-sched/src/cpu.rs` carries the derivation.

The mechanism is the same `ret` this sighting died at — `context_switch`'s, after
six pops and a `popfq` — and the diagnosis rests on a register the report above
does not carry: `rsi` still holding `new_rsp`, i.e. `rsp - 0x40`. **The one thing
that does not transfer is the frame.** Every one of the four restored a frame
with live values in it; this one restored eight zero quadwords, which
`OwnedAlloc::new`'s `alloc_zeroed` produces and a stack another CPU is walking
does not. So the reissued-kernel-stack reading in §1 above still stands on its
own evidence, and the first fault's cause is still what this file wants.

### What remains open

- **The first fault's cause.** Everything above says the `rip=0` is downstream
  of a recovery, and the recovery is downstream of a fault whose report this
  file does not carry. Without it there is nothing to fix in the port code, and
  the port code has now been audited and found sound on the two questions that
  were asked of it.
- **Whether the poison set's single slot is the whole of the collateral.** The
  separate issue tracks that.
- Do not close this on green runs.
