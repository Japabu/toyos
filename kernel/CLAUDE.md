# Kernel

The module header at the site owns its subsystem — this tree documents at the site, so read the header before changing a module. The scheduler core is `toyos-sched/`, driven from `kernel/src/sched/`; every Ring 3 transition's machine state is `kernel/src/arch/fpu.rs`; every syscall is `kernel/src/arch/syscall.rs`.

## Caveats that bite every agent

- **Nothing on the idle path touches a filesystem** — `log_file.rs` was the last one and it is gone; a log condition on the pre-`hlt` list is the scaffolding that mechanism replaced. The deletion ledger and review hold this; no gate does.
- **Anything added to the idle loop is an audio change** — housekeeping runs before `pass()`, so a woken CPU is late by what it costs. On a machine with nothing to run the idle loop does not run at all, so a diagnostic placed there reports nothing exactly when it is needed.
- **The idle loop may not take a global lock unconditionally** — the crash report reads the process table through a `try_lock` it must never block on, so housekeeping there is a standing aggressor against the one reader that cannot wait. `sched/reap_gate.rs` is the pattern: a relaxed-load gate in front of the lock.
- **`BackendGuard` masks interrupts for its whole life, so anything written under it is an interrupt latency** — the console drain is bounded to eight records and a userland `write` to one 1024-byte flush for that reason, and a new holder must bound itself too.
- **No disk wait in this kernel can park, and the driver is not why** — at the moment a transfer is waited for, the CPU is four ticket spinlocks deep, each disabling preemption for its whole life.
- **`drain_irqs` is the drivers' engine and nothing on it may wait** — a syscall that reaches it makes that thread drive USB enumeration, and a blocking call there empties the audio pipeline on every plug.
- **A console is per holder, minted at spawn** — `build_child_handles` gives a child its own `ConsoleObject` rather than duplicating its parent's handle, because the object *is* the line buffer. `console_line_atomicity` is the gate.
- **`ops::close` cancels a poll only for a source its object really ends** — `remove_fd` walks a source's watcher list across every ring in the machine, so a `SysCap` or `Console` close would otherwise cancel every log or keyboard poll there is. `ops::ends_its_sources` is where a new object kind answers.
- **Pressing Ctrl+Alt+D destroys the evidence it reports on** — the request reschedules every CPU, so a machine frozen on an unfired deadline reports `0 overdue`. Capture `info registers -a` over QMP *first*; `kernel/src/sched/dump.rs` explains the report.
