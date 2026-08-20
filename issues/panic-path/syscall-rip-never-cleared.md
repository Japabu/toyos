---
status: open
kind: defect
opened: 2026-07-31
---

# `percpu.syscall_rip` is never cleared, so "in syscall context" is a guess

`syscall_entry` stores the user RIP at `gs:[216]` on every SYSCALL and nothing
ever zeroes it. The panic handler's recovery predicate is `syscall_rip() != 0
&& current_tid().is_some()` (`main.rs`), so on any CPU that has ever served a
syscall the first half is permanently true. A panic in IRQ context — a timer
tick, a scheduler assert — with any task current is therefore treated as a
syscall panic: `try_recover_from_panic` poisons that task, kills the process
and rejoins the scheduler.

The consequence is backwards from fail-fast: a kernel bug with nothing to do
with the current process kills an innocent process and lets the machine run on,
instead of halting and reporting. `crash_report_panic` prints a "Syscall:
num=... user_rip=..." block off the same stale value, so the report also names
a syscall that is not running. Clearing it on syscall return is one store; the
honest predicate is a per-CPU "in syscall" depth.

## 2026-08-20: the block does not merely lie, it truncates the report

Measured, in a 12-wide boot storm capture of
`issues/kernel/a-btreemap-panicked-inside-its-own-insert-in-a-scheduler-pass.md`:

```
[kernel 0.557 cpu0 tid=1]   Syscall: num=90 user_rip=0x1000003d458 user_rsp=0xfffe8007b0
[kernel 0.557 cpu0 tid=1]   User backtrace:
[kernel 0.558 cpu0 tid=1] FAULT rip=0xffff80007cb37262 cr2=0x0 err=0x0 … RECURSIVE
```

The block is not only printed off a stale value — `user_backtrace` then walks a
stale `syscall_rbp` through the *current* address space, faults, and everything
the crash report had left to say goes with it. That is the last section of
`crash_report_panic`, so on this occasion nothing was lost; anything added after
it would be. The fix for that ordering is already taken (`report_contexts` is
emitted ahead of both backtraces), which is a placement and not a repair: the
stale word is still what sends the walk into a page that is not there.
