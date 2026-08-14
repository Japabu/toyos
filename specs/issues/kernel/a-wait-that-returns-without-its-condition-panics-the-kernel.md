---
status: open
kind: defect
opened: 2026-08-14
---

# `sys_process_wait` panics the kernel when its wait returns without the condition

`scheduler::wait_until` checks its predicate **once** and then blocks, and it
does not re-check on the way out:

```rust
// kernel/src/scheduler.rs:190
pub fn wait_until(queue: &KWaitQueue, deadline: u64, ready: impl Fn() -> bool) {
    let ticket = prepare_wait(queue);
    if ready() {
        ticket.cancel();
    } else {
        block_on(ticket, deadline);
    }
}
```

So any wake that is not this queue's own returns from a wait whose condition is
still false. `sys_process_wait` then treats the return as the condition:

```rust
// kernel/src/arch/syscall.rs:1439
crate::scheduler::wait_until(&queue, 0, || object.finished());
object.exit_code().expect("a finished process has an exit code")
```

**A userland `Child::wait()` therefore panics the kernel**, which is the line
root `CLAUDE.md` draws first: *"The kernel never crashes from userland."*

The publication side is not the bug. `ProcessObject::publish_exit` fills the
`exit` slot under its lock and only then stores `finished` with `Release`, so
`finished() == true` does imply `exit_code().is_some()`. What the `expect`
assumes is that the wait returned *because* of that store, and nothing makes
that true.

## Observed

`cargo test`, dev host, 2026-08-14, on `wt/toyos-logd` at `4ee9351`. One
occurrence in four full-suite runs of that tree; the three before it were
252/252.

```
PANIC: panicked at src/arch/syscall.rs:1441:10:
  Backtrace:
    core::panicking::panic_fmt+0x2c
    core::option::expect_failed+0x34
    kernel::arch::syscall::sys_process_wait+0x288
    kernel::arch::syscall::syscall_handler+0x612
  Running: pid=98 tid=Some(Tid(0))
  Syscall: num=108 user_rip=0x1000009596c
  User backtrace:
    toyos_abi::syscall::process_wait+0x1c
    <std::process::Child>::wait+0x3f
    process_lifecycle::main+0x2aa
```

The harness re-ran it alone and it passed, and adjudicated the *classification*:
*"ALONE process_lifecycle: GREEN — it fails only beside other guests, so its
`Sched::Parallel` is wrong."* That is a true statement about the schedule and a
misleading one about the cause: what other guests change is timing, and timing
is what decides whether an unrelated wake lands inside the window. The test is
the reporter, not the subject.

`cargo run -- --known-red process_lifecycle` says **NOT ON THE LIST**, so no
measured rate exists for it.

## Not the log branch's

Nothing in `wt/toyos-logd` touches process teardown, the wait queues or the
syscall. What that branch does change is the width of the interrupts-off window
in `emit` — it now reads the clock inside it — so it moves timing, which is a
plausible reason this surfaced on that tree and not a reason it lives there.

## The shape of a fix

`wait_until` should loop until its predicate holds, which is what every caller
already believes it does — and it is the caller-side half of the same rule the
tree applies to condition variables everywhere else. The `expect` in
`sys_process_wait` is then true by construction rather than by hope; if the loop
is refused for some reason, the `expect` must become a refusal (`WouldBlock` or
a retry) because it is reachable from userland either way.

Its siblings deserve the same read. There are **seven** call sites besides the
definition (`grep -rn 'wait_until(' kernel/src/`, 2026-08-14) — pipe space, pipe
data, virtio-sound, HDA, the keyboard, this one and `syscall.rs:1823` — and each
is a place where a spurious wake is currently indistinguishable from the
condition. Only this one turns that into a kernel panic; the others return early
and answer with whatever the un-met condition produces, which is its own class
of wrong answer.
