---
status: open
kind: defect
opened: 2026-08-14
---

# A thread's exit wakes only the main thread, so a join by any other thread waits for nothing

`process::thread_exit` posts exactly one wake, and it is always to the same
thread:

```rust
// kernel/src/process.rs:1315
let parent_main_tid = release_thread(process_pid, tid, code);
scheduler::wake_task(TaskId(process_pid, parent_main_tid));
```

`release_thread` returns `proc.main_tid` (`kernel/src/process.rs:1354`), so the
target is the process's main thread whatever the exiting thread was and whoever
was waiting for it.

`sys_thread_join` (`kernel/src/arch/syscall.rs:2137`) parks in the by-name
parking lot, which `sched::waitqs` documents as **never woken as a queue**:

```rust
let queue = crate::scheduler::park_lot();
loop {
    let ticket = crate::scheduler::prepare_wait(queue);
    match process::wait_thread_zombie(tid, caller) {
        Ok(Some(_)) => { ticket.cancel(); return 0; }
        Ok(None) => crate::scheduler::block_on(ticket, 0),
        Err(()) => { ticket.cancel(); return SyscallError::NotFound.to_u64(); }
    }
}
```

`wait_thread_zombie` registers the caller nowhere — it reads the table under its
lock and answers — so the only thing that can end that park is a by-name wake of
the *joining* thread. **A non-main thread joining a sibling gets none.** It
sleeps until some unrelated wake happens to reach it and its own re-check finds
the zombie, and on a process where none ever does, it sleeps forever.

The re-check loop is what makes this a hang rather than a wrong answer, and it
is also why nothing has tripped over it: `std::thread::JoinHandle::join` calls
this syscall (`rust/library/std/src/sys/thread/toyos.rs`), and every join in the
tree today is made from a main thread, which is the one thread the wake reaches.

## What a fix has to decide

Not "wake more threads". The wake is by name because a join has no queue, and
the shape that fits the rest of the kernel is the one `ProcessObject` already
uses for processes: the *thread* owns a wait queue, every joiner registers on
it, and the exit wakes it. `wake_task(main_tid)` would then have nothing left to
do — it is only there because the main thread was assumed to be the joiner.

Found while fixing `scheduler::wait_until`'s missing re-check
(`a-wait-that-returns-without-its-condition-panics-the-kernel`), which is the
same family read from the other end: that one is a wait ended by a wake meant
for something else, this one is a wait that is owed a wake nobody sends.
