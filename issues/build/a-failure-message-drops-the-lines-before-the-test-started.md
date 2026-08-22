---
status: open
kind: defect
opened: 2026-08-17
---

# A failure message drops `TestResult::before`, which on a pre-marker death is the only record there is

`run_test_paced` files every console line a test's window carried into one of
two fields. Lines after `===TEST_START <name>===` go to `TestResult::serial`;
lines before it go to `TestResult::before`, whose own doc comment argues at
length that it must not be dropped. **Nearly every caller drops it.** A failure
arm formats `result.error` and `result.serial` and nothing else, so a guest that
died before it announced the test has its cause filed in a field the report does
not print.

That is not hypothetical and the instance is unrecoverable. `sched_check_build`,
run `31890991692`, job `95027203184`, `guest 8`, 2026-08-15, printed an **empty**
`serial:` block: `in_test` never became true, so every console line that boot
produced — a panic among them, if there was one — went to `before`, and the
`sched_check_build` arm formats only `result.serial`. The uploaded
`shard-N-serial` artifact does not cover it either: the 16550 log ends where the
kernel switches to virtio-console, at 0.377 s of guest uptime. That sighting
stays undecided for exactly this reason — the harness deliberately does not
read the run's other, decided sighting (a confirmed invariant-P panic on the
same test name, two days later) across onto it.

The shape of the fix: every caller that reports `result.error` reports `before`
and `started` with it. `started` is what tells the reader which of the two
fields the evidence was ever going to be in.

## What this is no longer

Filed as the second half of *a kernel Rust panic during a test reads as a
stall*, whose first half — the two waits disagreeing about what a fatal line
looks like — is fixed: `tests/common/serial.rs` holds one vocabulary and
`run_test_paced`, `wait_for_ready` and `await_guest` all read it. That fix makes
a *post*-marker death name itself in `result.error`, which is where most of them
land. It does nothing for a death before the marker, which is this file.
