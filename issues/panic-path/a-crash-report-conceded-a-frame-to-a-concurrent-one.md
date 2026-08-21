---
status: open
kind: defect
opened: 2026-08-21
---

# Two crash reports at once, and one conceded a frame to the other

`panic_recovery` reded on the T14 guest lane of run `32527751613`
(job `96913340222`, 2026-08-21 21:25Z), green when the harness re-ran it alone:

```
FAIL rs::panic_recovery: the crash report could not read a symbol it was asked
for, so a bare address in it is a lost race and not a verdict:
[kernel 3.015 cpu0]     0x100000072ce  <symbol unread: the process table was held>
FAIL panic_recovery: exit code Some(0)
```

`tests/toyos.rs`'s `check_symbols_were_read` is what reds, and its own doc
comment says why that is worth a red: `scheduler::reap_poisoned` no longer takes
`PROCESS_TABLE` on every idle trip, so `<symbol unread: …>` "names a lock holder
worth finding rather than the housekeeping that used to be there"
(`kernel/src/process.rs`, `with_user_symbols`).

## The holder is in the same log

The concession is one frame of one report, and the frames around it resolved:

```
[kernel 3.014 cpu0]   User backtrace:
[kernel 3.014 cpu0]     0x10000006546  toyos_abi::syscall::debug+0x16
[kernel 3.014 cpu0]     0x10000001248  test_panic_child::main+0x48
        ... five more frames, all named ...
[kernel 3.015 cpu0]     0x100000072ce  <symbol unread: the process table was held>
```

That last address is `_start+0xe`; **the same address resolved by name three
milliseconds earlier** in cpu1's report of the previous child
(`[kernel 3.012 cpu1]     0x100000072ce  _start+0xe`). So the symbol table
covers it and the tables were not the problem — one `try_lock` lost.

What else was running at 3.014–3.017 s is in the same serial: cpu1 was in the
panic path itself, `PANIC: panicked at src/arch/syscall.rs:699:17` at 3.017,
while cpu0 was rendering a `SEGFAULT` report for pid 60 from
`syscall_handler+0xb00`. The test spawns child after child into a deliberate
panic, so **two crash reports overlapping is this test's ordinary weather**, and
a report that walks the process table while another holds it is the shape.

`with_user_symbols` takes the answer at once and carries the reason out rather
than waiting, deliberately and for a reason its doc states in full — the
faulting thread may hold the lock itself, so a wait is a deadlock on the one
path that must always produce output. **So the fix is not a retry here.** The
question this leaves is which holder it is: a second concurrent report, the
reaper, or the spawn path, and whether a report can be given a way to symbolise
that does not go through the process table at all.

## What it costs

A rate, not a classification: one observation, and the re-run alone was green.
Nothing in the run's diff (a workflow, a `const`, doc comments and issue text)
reaches a boot. The cost is that a crash report — the artifact the panic path
exists to produce — silently loses a frame's name whenever another CPU is
reporting at the same time, which is exactly when a reader most needs it.
