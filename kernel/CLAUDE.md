# Kernel

The law lives in the specs: `specs/user-machine-state.md` (every Ring 3 transition), `specs/capability-endowment-spec.md` (handles, refusals, endowment), `specs/input-architecture.md` (the kernel's input half), `specs/iommu-spec.md`, `specs/audio-subsystem-spec.md` (its kernel-interface section), `specs/scheduler-core-spec.md` (the core, which is `toyos-sched/`, driven from `kernel/src/sched/`). Where no spec owns a subsystem, the module header at the site does — this tree documents at the site, so read the header before changing a module. Every syscall is `kernel/src/arch/syscall.rs`.

## Caveats that bite every agent

- **Anything added to the idle loop is an audio change** — housekeeping runs before `pass()`, so a woken CPU is late by what it costs. On a machine with nothing to run the idle loop does not run at all, so a diagnostic placed there reports nothing exactly when it is needed.
- **The idle loop may not take a global lock unconditionally** — the crash report reads the process table through a `try_lock` it must never block on, so housekeeping there is a standing aggressor against the one reader that cannot wait. `sched/reap_gate.rs` is the pattern: a relaxed-load gate in front of the lock.
- **`BackendGuard` masks interrupts for its whole life, so anything written under it is an interrupt latency** — the console drain is bounded to eight records and a userland `write` to one 1024-byte flush for that reason, and a new holder must bound itself too.
- **No disk wait in this kernel can park, and the driver is not why** — at the moment a transfer is waited for, the CPU is four ticket spinlocks deep, each disabling preemption for its whole life. `specs/plans/blocking-io-plan.md` is the wave; `specs/issues/audio/disk-wait-pins-a-cpu.md` the entry.
- **`drain_irqs` is the drivers' engine and nothing on it may wait** — a syscall that reaches it makes that thread drive USB enumeration, and a blocking call there empties the audio pipeline on every plug.
- **Pressing Ctrl+Alt+D destroys the evidence it reports on** — the request reschedules every CPU, so a machine frozen on an unfired deadline reports `0 overdue`. Capture `info registers -a` over QMP *first*; `kernel/src/sched/dump.rs` explains the report.
