# Diagnostics roadmap

Three layers, built in order.

1. **Process accounting** — cumulative per-process wall and CPU time, page faults by cause, I/O ops and bytes, time blocked by reason. Built; the syscall reports only an exited direct child, exactly once.
2. **Event tracing** — per-process ring of `(timestamp_ns, TraceEvent)`, ~24 bytes/event, one syscall to read. Answers "where is time going, in what order?".
3. **RIP sampling** — needs frame-pointer unwinding to be worth anything; build only once 1–2 confirm something is CPU-bound.
