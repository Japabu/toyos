---
status: open
kind: defect
opened: 2026-08-14
---

# `nanosleep` ends early when a sibling thread exits

`sys_nanosleep` parks once with a deadline and returns on whatever ends the
park:

```rust
// kernel/src/arch/syscall.rs:2289
fn sys_nanosleep(nanos: u64) -> u64 {
    let deadline = crate::clock::nanos_since_boot().saturating_add(nanos);
    // No condition to re-check: the deadline is the wake, and one that has
    // already passed fires at the next scheduler entry.
    crate::scheduler::block_on(
        crate::scheduler::prepare_wait(crate::scheduler::park_lot()),
        deadline,
    );
    0
}
```

The comment is the bug: the deadline is *a* wake, not the only one. A parked
task is also woken by name, and `process::thread_exit` posts exactly such a wake
to its process's main thread on every child thread's exit
(`kernel/src/process.rs:1315`). A main thread sleeping in `nanosleep` therefore
returns as soon as any thread of its process exits, having slept for as long as
that took and no longer. `std::thread::sleep` is one call to this and does not
loop (`rust/library/std/src/sys/thread/toyos.rs`), so the short sleep reaches
userland whole.

## What is measured and what is derived

Measured: that the by-name wake really does reach a *parked* main thread and end
its wait. That is what
`tests/toyos-rust-tests/src/bin/process_lifecycle.rs`'s
`an_unrelated_wake_does_not_end_the_wait` provokes, and against the tree before
`scheduler::wait_until` looped it ended the wait in `sys_process_wait` and
panicked the kernel — twice out of two runs, parallel and alone, on this host on
2026-08-14.

Derived, not measured: that the same wake shortens a `nanosleep`. Nothing has
timed one. The park is the same park and the wake is the same wake, so the
mechanism is not in doubt; what has not been established is how short, how
often, and whether anything in the tree sleeps in a thread that is the target.

## Shape of a fix

The same shape `scheduler::wait_until` now has and `io_uring::wait` already had:
re-park until the clock reaches the deadline. There is no condition to re-check
here, so the loop is over the clock alone, and it must stay a `block_on` with
the *absolute* deadline so the sleep ends no later than it was going to.

Whether an early return should ever be visible is a separate question this does
not answer: there are no signals in this kernel, so `nanosleep` has nothing to
report and no reason to end early at all.
