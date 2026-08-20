---
status: open
kind: defect
opened: 2026-08-19
---

# A deferred release can finish after the syscall that caused it has returned

`object::drain_zero_handles` (`kernel/src/object/mod.rs`) takes the whole queue
and clears `ZERO_PENDING` **before** it runs a single hook:

```rust
let batch = {
    let mut queue = ZERO_QUEUE.lock();
    ZERO_PENDING.store(false, Ordering::Release);
    core::mem::take(&mut *queue)
};
for object in batch {
    object.run_zero_handles();
}
```

So "the queue says empty" is not "the work is done", and the CPU that *queued*
an object is not guaranteed to be the one that releases it. The drain runs at
three sites — syscall exit, `do_schedule` entry, the idle loop — and any of the
other two, on any other CPU, can take a batch out from under the syscall that
filled it. That syscall then reaches its own drain site, is told the queue is
empty, and returns to userland with its objects still unreleased.

**Measured 2026-08-19 on `wt/toyos-fdleak` at `8e9f851`**, `tests/testcases`,
two CPUs, TCG, one binary in the guest. `handle_lifetime`'s holder makes eight
io_uring rings (16 MiB) and is killed; the killer reads `SYS_SYSINFO` eight
times back to back straight after `wait` returns. The deficit against the
pre-spawn reading, in megabytes, over those eight reads:

```
round 1  [12, 10, 10,  8,  6,  6,  6,  6]
round 3  [12, 10, 10, 10,  8,  6,  4,  2]
round 5  [10, 10,  8,  6,  4,  2,  2,  2]
round 9  [14, 12, 10, 10, 10,  8,  6,  4]
round 13 [14, 14, 12, 10, 10, 10, 10,  8]
```

Ten of twenty rounds decayed like that; the other ten read zero on the first
try. It is a 2 MiB staircase — one ring page at a time — which is the other
CPU working through the batch while this one reads.

**Nothing is lost.** Over the same twenty rounds free memory returned to its
starting value every time; the drift against the round-0 baseline was zero at
every round. A kernel trace confirms the shape from the other side: with
`RingRef::drop`, `drain_zero_handles` and `SYS_PROCESS_KILL` logged, all eight
`RingRef` frees land in a `batch=9` drain that runs *after* `kill_process` has
returned, and a second CPU was caught taking a batch mid-kill —

```
[cpu0] KILLPROBE enter target=15 t=552074985
[cpu1] ZQPROBE drain batch=1 t=552533113
[cpu0] KILLPROBE done  target=15 t=554353621
```

## A third witness, and it is not a free-memory verdict

`handle_kill_policy` reds on the same mechanism through a completely different
instrument — the per-kind object census, not `SYS_SYSINFO`. Seen on this branch
in a full twelve-wide suite, 2026-08-19, at `bb6893c`:

```
16 more killed processes left more live objects behind:
  [("SharedMem", 5, 6), ("Process", 6, 7)]
```

One `SharedMem` and one `Process` still alive at the closing reading. That is
the same "the release has not run yet" and not a leak: `SharedMem` is a
`deferred` row released from `ZERO_QUEUE`, and a `ProcessObject` outlives its
table entry until `reap_finished` takes it — which runs from the idle loop
under `IdleProof`, so it is a second asynchrony of the same class with a longer
tail. The census is immune to *another binary's churn*, which is what the
free-memory verdicts are not, and it reds anyway. So the shared boot was never
the common factor between these three names; the release latency is.

`handle_kill_policy` is on the redlist already (`src/redlist.rs`, dev host
loaded, 1 of 3, 2026-08-18) with a contention reading, and it is **not touched
here** — this entry records the mechanism, and re-adjudicating that row is its
owner's to do with a measurement rather than with this argument.

## A syscall answering the wrong word, 2026-08-20

**The three witnesses above are quantities that settle. This one is not.**
`kill_while_blocked` kills a child parked in a blocking read and asks the peer
end whether it knows; the answer must be `NotFound` and is `Ok(22)`.
`issues/kernel/a-killed-peer-still-takes-a-write.md` carries the reproduction,
and the chain is this queue end to end: the victim's handle goes at
`ops::close_all`, `HandleEntry`'s drop queues the object, and the object's
`on_zero_handles` — `PipeReadEnd`'s or `ConnectionEnd`'s — is the only thing
that calls `Held::release` and gives the `PipeReader` back. `pipe.readers` is
what `pipe::try_write` reads, and it is still 1 while the batch is in flight on
another CPU, so the write is accepted into a ring nobody will ever read.

Measured on `e4c2c8ff`, dev host: **2 red of 53** one-name runs, one on each of
the two arms that ask a peer; **4 of 5** with
the syscall-exit drain removed, which stages the same state a stolen batch
leaves. The trace is a kill returning on cpu0 at 0.542 s and the victim's read
end being released on cpu1 at 0.544 s.

So the sentence below is no longer the whole of it: this is a *semantic* event
riding a release the caller cannot wait for, which is what
`kernel/src/object/mod.rs`'s own header says must never happen — *"every
userland-visible lifecycle event rides `handle_count`"* — and the same shape
reaches soundd, whose cpal clients spend their lives parked in a signal-pipe
read.

## Why it matters beyond a test

Two harness binaries had to learn to settle (`handle_lifetime`,
`shm_release_reclaims`; `issues/build/free-memory-verdicts-share-a-boot.md`
carries that story). The consequence that is not a test is a process which kills
a child to make room and immediately allocates: the pages it just freed are not
free yet, and `SYS_SHM_CREATE`/`io_uring_setup` can answer `ResourceExhausted`
for memory the machine is in the middle of handing back. On a memory-tight
machine that is a spurious refusal, and nothing in the ABI lets the caller tell
it from a real one.

`ops::close_all`'s own doc states the intent this misses — *"Called by exit **and
by kill**, so the drops below are on the path a process taken down by another
CPU follows"* — which is about the drop happening, not about it having finished.

## What to do

Not "drain harder": every drain site already runs, and adding a fourth changes
nothing about a batch another CPU is holding. The two honest shapes are

- **Never publish a batch as absent while it is in flight.** Popping one object
  at a time and clearing `ZERO_PENDING` only when the queue is genuinely empty
  shrinks the window from "every object the kill queued" to "at most one per
  other CPU" — a mitigation, not a guarantee, and at four vCPUs three 2 MiB
  pages is still a visible amount.
- **Give the batch an owner.** The releasing syscall should run the hooks of the
  objects *it* retired, with nothing held, before it returns — which is what
  makes the kill path and the exit path one teardown rather than two, and what
  makes "a killed process holds nothing" a fact rather than a race.

The second is right and it is **not free-standing work**: it is the object
layer's release protocol, and the track that owns it is
`issues/kernel/every-wait-in-this-kernel-is-a-spin.md` — its one park site, its
cancellable kill and its sleep lock between them decide what a hook released
from this queue is allowed to do. The constraint that track's own reasoning
derived, and which anything touching this queue must not lose: **none of the
three drain sites can park, so no `on_zero_handles` hook may take a sleep lock
at all.** It belongs with that track, not beside it.

### What "give the batch an owner" costs, worked out 2026-08-20

An owner has to be the *thread*, not the CPU. `kill_process` phase 2 calls
`scheduler::retire_task`, which parks the killer until the victim's record is
released, so the killing thread can be moved to another CPU between the
`close_all` that queues its objects and the syscall exit that would drain them —
a per-CPU list would strand exactly the batch it was added to own.

A per-thread list cannot live behind `ThreadData`'s lock either:
`teardown_resources` holds `ProcessData` across `close_all`, and its own first
line is that the two locks are never held together. So the list has to be on the
kernel stack, threaded through `close_all` to a caller that runs the hooks once
its guard is gone.

**That is the part that is not a patch.** `HandleEntry`'s drop is where the
enqueue happens today, and the comment on that statement is the whole argument
for the design — *"this is the one statement that makes 'a hook cannot run under
a lock' structural"*. A `close_all` that hands its objects back for the caller
to run re-opens, for that one call site, precisely the guard-outlives-the-drop
trap the queue exists to make unwritable. Any owner-shaped fix has to pay for
that property somewhere else rather than spend it, which is why this is the
track's work and not a fix at the site.

`kernel-loom` is not the instrument for it, and that is worth writing down so
nobody re-derives it: the models compile the real kernel files with
`feature = "loom"`, and `object/mod.rs` pulls in `alloc::sync::Arc`, the whole
`kobject!` set and every subsystem those hooks reach. A transliteration is what
that crate's header exists to refuse.

That is the cost on the `deferred` rows. The `immediate` rows have their own,
and the section "What the sleep lock decided, 2026-08-20" is it.

## What the sleep lock decided, 2026-08-20

That track's lock-conversion pass reached this and could not answer it, and why
is worth recording here, because it changes what "give the batch an owner" has
to cover.

**The kind that most needs a release site is `File`, and `File` is not on this
queue.** `object/mod.rs` makes it an `immediate` row deliberately — *"A file's
flush and its cache reference ride the last `Arc`"* — so its release is
`OpenFileState::drop` (`kernel/src/object/file.rs:27-39`), which takes
`vfs::lock()` and flushes. Once `vfs::VFS` is a sleep lock that `Drop` has no
legal way to acquire it: a `Drop` impl cannot be handed a `Parkable`, and the
two contexts it actually runs in — `ops::close` inside
`process::with_process_data` (`arch/syscall.rs:1250`) and `ops::close_all`
inside `teardown_resources`'s Phase 2 (`process.rs:1126`–`1149`) — hold a
`Lock<ProcessData>` guard, so the baseline assertion refuses one level before
the discipline rule does. `try_lock` is not an answer either: the holder it
would lose to is a thread inside a device round trip, so the failure is routine,
and what it loses is a modified file's write-back.

So the two constraints meet. **The hook queue may not park, and the row that
would want to is not on the hook queue but in a `Drop` that also may not.**
Moving `File` to `deferred` swaps one illegal site for another. The second shape
above is still right for the `deferred` rows, and by itself it reaches neither
`File` nor `close_all` — that one is also called from `recover_or_halt`'s
`Blame::Process` arm (`arch/idt/exceptions.rs:348`), which has no syscall to
return through.

The track carries this as **wall 4**, with the three shapes the owner has to
choose between. Nothing here should be built before that choice, because all
three of them move this queue.
