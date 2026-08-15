---
status: open
kind: defect
opened: 2026-08-15
---

# `thread_exit` unwraps a process-table entry its own neighbour documents as raceable

`process::thread_exit` asks whether the exiting thread is the main one, and
takes the answer without checking that the entry is still there:

```rust
// kernel/src/process.rs:1334-1338
let is_main_thread = {
    let guard = PROCESS_TABLE.lock();
    let table = guard.as_ref().unwrap();
    table.get(process_pid).unwrap().main_tid == tid
};
```

The second `.unwrap()` is on `table.get(process_pid)`. A thread reaching here
whose process was already reaped — by a concurrent `kill_process` on another CPU,
which removes the entry — panics the kernel.

Two things make this worth a line rather than a shrug.

**The neighbouring function documents exactly this race and tolerates it.**
`mark_thread_zombie` (`kernel/src/process.rs:829`) carries the argument at
`:826-828`: it is *silent about an entry that has gone*, because a main thread
reaches it after its own process published its exit. Two functions on the same
teardown path disagree about whether the entry may be missing, and only one of
them is written for the answer it gives.

**The branch it guards is unreachable from anything shipped, so the panic is the
only thing this code can currently do that is observable.** Every
`SYS_THREAD_EXIT` issuer in the tree sits inside a spawned-thread trampoline —
`rust/library/std/src/sys/thread/toyos.rs:63` and
`userland/libc/src/pthread.rs:70` — and a main thread returns into `rt` and
leaves through `SYS_EXIT` → `process::exit`. So on today's tree nothing takes the
`is_main_thread == true` path at all; what remains reachable is the `.unwrap()`
in front of it, on every non-main thread exit, racing a kill.

## Related, and not to be conflated

`specs/plans/kernel-slimming-roadmap.md` proposes deleting the main-thread-exit
policy itself (*"a process ends when its last thread does, or only by explicit
`SYS_EXIT`"*). That is a design change with its own cost — the survivor needs a
last-thread-exit teardown trigger, or a raw `SYS_THREAD_EXIT` from tid 0 leaks
the main thread's 8 MB stack and never publishes an exit, so every
`SYS_PROCESS_WAIT` waiter parks forever. **This entry is not that.** It is the
`.unwrap()`, which is wrong whether or not the policy stays.

`specs/issues/kernel/a-thread-exit-wakes-only-the-main-thread.md` is a third,
separate defect in the same function.

## What a fix owes

Match `mark_thread_zombie`: a missing entry means the process is already gone, so
the thread is not the main one in any sense that matters and the branch is
skipped. If instead the entry genuinely cannot be missing here, then
`mark_thread_zombie`'s comment is the false one and the two should be made to say
the same thing — but that claim needs the argument, and today neither function
carries it.
