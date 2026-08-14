---
status: open
kind: defect
opened: 2026-08-14
---

# `sys_process_wait` panics the kernel when a finished process has no exit code

`kernel/src/arch/syscall.rs:1441` reads the process object twice and nothing
makes the first read imply the second:

```rust
crate::scheduler::wait_until(&queue, 0, || object.finished());
object
    .exit_code()
    .expect("a finished process has an exit code") as u32 as u64
```

Under load the wake happens with `finished()` true and `exit_code()` still
`None`, and the `expect` takes the kernel down. That is the right thing for a
broken invariant to do — a kernel bug crashes loudly — so the defect is the
invariant, not the panic: `finished()` and `exit_code()` are two separate reads,
and whatever publishes the second is not ordered against whatever publishes the
first.

## What was seen, 2026-08-14

A full `cargo test` on the dev host, in a worktree at main `2a1449e` whose diff
is `src/toolchain.rs` and two workflow files — nothing the kernel is built from.
One test of 252 failed:

```
FAIL rs::process_lifecycle: exit code -1
  the code is read three times and is the same each time
a finished process has an exit code

[kernel 12.243 cpu0 tid=0] exit: test_rs_process_lifecycle tid=1 code=0 cpu=1ms
[kernel 12.243 cpu1 tid=0] !!! PANIC !!!: panicked at src/arch/syscall.rs:1441:10:
[kernel 12.244 cpu1 tid=0]     core::option::expect_failed+0x34
[kernel 12.248 cpu1 tid=0]     kernel::arch::syscall::sys_process_wait+0x288
[kernel 12.249 cpu1 tid=0]     kernel::arch::syscall::syscall_handler+0x5f1
[kernel 12.249 cpu1 tid=0]   Running: pid=98 tid=Some(Tid(0))
[kernel 12.249 cpu1 tid=0]   Syscall: num=108 user_rip=0x1000009597c
[kernel 12.249 cpu1 tid=0]     toyos_abi::syscall::process_wait+0x1c
[kernel 12.250 cpu1 tid=0]     <std::process::Child>::wait+0x3f
[kernel 12.250 cpu1 tid=0]     process_lifecycle::main+0x2aa
```

The waiter is pid 98 on cpu1; a thread of the same program exits on cpu0 in the
same millisecond, and pid 100 is logged as `exit: … code=7` immediately after
the panic. Three processes of one program were live across two CPUs.

## What is known about it

- **It is load-coincident, not deterministic.** Three isolated re-runs
  (`cargo test --test toyos-build -- --host-slots 0 process_lifecycle`) passed,
  319/329/327 ms. It failed once inside a full parallel run whose log carries
  `[build-lock] waiting` around it. Host load is not an excuse: this is a race
  that a busy machine exposes, not noise.
- **No rate has ever been measured.** `cargo run -- --known-red
  process_lifecycle` answers `NOT ON THE LIST`, so it is not adjudicated and
  cannot be re-run away.
- `specs/issues/diagnostics/process-stats-exited-child-only.md` is about the
  same syscall family and is a different question — that one is about what
  `process_stats` can see, not about a wake that arrives too early.

Found while landing three build-system fixes; filed rather than fixed, because
it is nowhere near that diff.
