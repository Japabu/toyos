---
status: open
kind: defect
opened: 2026-08-15
---

# `io_uring_enter` trips "a task waits on at most one queue" in logd

On a KVM shard, in the boot `usb_boot_stick_pulled` stages, the kernel panicked
in logd's syscall:

```
[kernel 8.982 cpu2] PANIC: panicked at /__w/toyos/toyos/toyos-sched/src/waitq.rs:124:9:
a task waits on at most one queue
[kernel 8.982 cpu2]   Backtrace:
[kernel 8.982 cpu2]     <toyos_sched::waitq::WaitQueue<…>>::prepare_wait+0x1bc
[kernel 8.982 cpu2]     <kernel::sched::driver::Ticket>::register+0x9c
[kernel 8.982 cpu2]     kernel::io_uring::enter+0xe8f
[kernel 8.982 cpu2]     kernel::arch::syscall::sys_io_uring_enter+0x8e
[kernel 8.982 cpu2]   Running: pid=2 tid=Some(Tid(0))
[kernel 8.982 cpu2]   Process: logd pid=2 state=Live
[kernel 8.982 cpu2]   User backtrace:
[kernel 8.983 cpu2]     <toyos::poller::Poller>::submit+0x32
[kernel 8.983 cpu2]     <toyos::poller::Poller>::wait::<logd::main::{closure#2}>+0x11
[kernel 8.983 cpu2]     logd::main+0x9c3
```

**The stimulus is the device going away, not input.** The boot stick was pulled
at 4.297 s — `usb-storage: reset recovery failed; disk is offline`, then
`usb-storage: write of 1 blocks at 17409 failed on disk 0` — and logd had
already said
`/log has not answered (the sync: other error) - this boot's log is on the
console only from /log/2026-08-15-181448_0010.log`. The panic is 4.7 s after
that, in logd's next `Poller::submit`.

**It is not the path
`issues/kernel/keyboard-flood-panics-blocked-read.md` records.** That one
reaches the same assertion through
`scheduler::wait_until::<kernel::keyboard::has_data>` from `sys_read`, on a
thread blocked on stdin under thousands of injected key events a second. This
one has no keyboard in it at all: the waiter is a ring's completion queue and
the caller is `kernel::io_uring::enter`. Same invariant
(`toyos-sched/src/waitq.rs:124`, over `set_waiting()` in
`toyos-sched/src/task.rs`), two ways in — so the flag being left set is the
subject, and neither site is.

What the assertion says happened: this thread's task word still carried
*waiting* when `enter` prepared a new wait. `enter`'s loop consumes its ticket
on every exit it can see — `cancel()` on the error path, `cancel()` on the
satisfied re-check, `block_on` otherwise — so what is unaccounted for is a
previous wait of this thread that ended without clearing, and the pulled device
is what makes the ring's completions abnormal. Which wait that was is not
established here and a capture of one panic cannot establish it.

**The machine did not stop**, which is why this is a red and not a wedge: the
capture continues past the report to `pull-probe-91` at 13.795 s, with the
stick's re-insertion enumerating at 9.202 s in between. `usb_boot_stick_pulled`
refuses any post-pull capture carrying `PANIC:`, so the test names it.

Evidence, once: nightly dispatch `31900050723`, job `95049280131`
(`guest (3)`), `wt/toyos-ciwall`, 2026-08-15, in the serial phase.
`ALONE usb_boot_stick_pulled: GREEN, and it was alone both times — nothing the
harness controls differed, so it failed once and passed once. That is a rate
and not a classification.` The sibling dispatch `31900045901` (`main` at
`e064a96`) minutes earlier was green on this name, and the two trees differ
only in `src/testargs.rs`, `tests/toyos.rs` and one deleted issue file — the
kernel is the same kernel in the green run and the red one. A KVM shard runs
one guest per machine at `--jobs 1`, so host contention is not available as an
explanation either.
