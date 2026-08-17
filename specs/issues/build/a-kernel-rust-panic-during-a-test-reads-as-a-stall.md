---
status: open
kind: defect
opened: 2026-08-17
---

# A kernel Rust panic during a test reads as a stall, and costs the guard's whole ceiling

`tests/common/qemu.rs` has two waits on a guest, and they do not agree about
what a fatal line looks like.

`wait_for_ready` — the boot half — ends on `SEGFAULT`, `KERNEL PANIC` **or**
`PANIC:`, drains two more seconds so the backtrace is in the report, kills the
child and panics with `Init process crashed during boot`. (Armed when
`panic_aborts`, which is `ready == DEFAULT_READY`.)

`run_test_paced` — the half every `run_test` goes through — ends early on
`KERNEL PANIC` and nothing else.

`KERNEL PANIC` is written by `kernel/src/arch/idt/exceptions.rs:196-197` and
only there; it is the CPU-exception path. A Rust `panic!` in the kernel goes to
`crash_report_panic` in the same file, which writes `alert!("PANIC: {}", info)`
at line 272. So a `debug_assert`, an `expect`, or any `feature = "check"`
scheduler assert cannot end a `run_test` wait — and if the panic came from
inside a scheduler pass, `kernel/src/scheduler.rs`'s `schedule_no_return` calls
`halt_all_cpus()`, so the machine goes silent for the rest of the ceiling.

The wait then expires, `last_line` is old, and the failure is classified
`STALLED:`. The suite summary says *"N of those reds are blown liveness guards,
not answers … The guest stopped making progress, so the run established nothing
about this tree and there is nothing in it to bisect"*, and `durations` prices
the name at the guard's own arithmetic.

## The occurrences this was found on

Both are `sched_check_build` on 2026-08-16, and in both the capture printed
inside the `STALLED:` message carries the panic that caused it:

- Run 31946183485, job 95162423932, `guest (8)`: `STALLED: 382s of guard expired,
  and the guest had said nothing for the last 383s of it`, and four lines below
  it `invariant P: a scheduler pass took 200569 ns, budget 200000 ns` with its
  full kernel and user backtrace. The guest died at 1.450 s of its own uptime.
- Run 31936533470, job 95139261820, `guest (8)`, a push to `main`: `STALLED:
  387s`, `invariant P: a scheduler pass took 277260 ns`, dead at 1.140 s.

`specs/issues/kernel/the-check-build-guest-stopped-answering-on-kvm-twice.md` is
the adjudication; a survey of the 100 most recent `ci` runs puts the pair at 2 of
91 sampled runs, so this is not a once-off.

Cost, measured rather than estimated: those shards' parallel phases took 481.2 s
and 544.9 s for six tests whose other five measured 3–5 s each. The name is
priced at `sched_check_build 6635` in `tests/test-durations`; one run recorded
387502 ms for it.

## Why the naive patch is not the fix

Adding `PANIC:` to `run_test_paced`'s condition would copy `wait_for_ready`'s
bare substring into a wait that runs during *userland* execution, where a guest
binary's own Rust panic prints text a test may legitimately be asserting on.
The kernel's line is prefixed `[kernel <t> cpu<N>] `, so the safe predicate is
the kernel-prefixed form and not the substring.

`tests/common/serial.rs` already declares the vocabulary —
`const FATAL: &[&str] = &["PANIC:", "KERNEL PANIC", "panicked at"]` — and
nothing shares it with either wait. One definition of "the kernel said it was
dying", used by `Serial::must_be_clean`, by `wait_for_ready` and by
`run_test_paced`, is the shape of the fix.

This is the guard machinery every test in the suite runs through, so it wants
its own change, its own gate and a negative control that proves a guest which
merely goes quiet is still reported as a stall.

## The second half: `TestResult::before` is dropped where it matters most

The earlier sighting of the same test (run 31890991692, job 95027203184,
`guest 8`, 2026-08-15) printed an **empty** `serial:` block. `in_test` never
became true, so every console line that boot produced — a panic among them, if
there was one — went to `TestResult::before`, and the `sched_check_build` arm
formats only `result.serial`. The field's own doc comment already argues it must
not be dropped; on a failure path it is the only place a pre-marker death is
recorded, and the uploaded `shard-N-serial` artifact does not cover it either
(the 16550 log ends where the kernel switches to virtio-console, at 0.377 s of
guest uptime).

Every caller that reports `result.error` should report `before` and `started`
with it. That cause is unrecoverable now, which is the point.
