---
status: open
kind: defect
opened: 2026-07-31
---

# A keyboard flood into a blocked `sys_read` panics the kernel

`prepare_wait` asserts `set_waiting()`, "a task waits on at most one queue"
(`toyos-sched/src/waitq.rs:124`), and a thread blocked in `sys_read` on the
keyboard fd trips it under sustained input:

    !!! PANIC !!!: panicked at toyos-sched/src/waitq.rs:124:9:
    a task waits on at most one queue
      <WaitQueue<…>>::prepare_wait+0x1a5
      <kernel::sched::driver::Ticket>::register+0x9f
      kernel::scheduler::wait_until::<kernel::keyboard::has_data>+0x49
      kernel::arch::syscall::sys_read+0x77
    Running: pid=1 tid=Some(Tid(0))   Syscall: num=1

Seen twice while looking for something else: `Profile::MetalUsb` with QMP
key events injected across the whole boot at a few thousand a second, once
with the i8042 present and once with `q35,i8042=off`, so both the PS/2 and
the USB delivery paths reach it. The victim both times was the in-guest
test runner blocked on stdin at `===READY===`. It does not reproduce at
ordinary typing rates, and neither run was reduced further — the flag is
still set from a previous wait when `sys_read` loops round and prepares the
next one, but which of `wait_until`'s cancel/commit paths left it set was
not established. Reproducing it deliberately means a guest-side key
generator, not a host-side flood.
