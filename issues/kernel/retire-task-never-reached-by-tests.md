---
status: open
kind: defect
opened: 2026-07-31
---

# `retire_task` is never reached by `cargo test`

Instrumented across all 140 tests: zero calls. Threads that `join` are removed
from the table (`collect_thread_zombie`), so `process::exit`'s phase-2 retire
sweep finds nothing, and no test kills a live process. Both callers —
multi-threaded teardown with unjoined threads, and `kill_process` — are therefore
untested, including the message-plus-park protocol the scheduler rewrite rebuilt
them on.
