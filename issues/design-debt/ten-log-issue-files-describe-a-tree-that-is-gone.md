---
status: open
kind: defect
opened: 2026-08-18
---

# Ten `issues/` entries still describe the log subsystem the kernel no longer has

The log architecture landed on 2026-08-15: the kernel keeps a per-CPU record
ring and the console, `/bin/logd` owns `/log`, `kernel/src/log_file.rs` and
`kernel/src/drivers/log_ring.rs` are gone. The closing pass that was to retire
the entries that work answers never ran, so ten files still describe the byte
ring, the idle-loop flush and the kernel file sink as live.

Verified against the tree on 2026-08-18. Each row says what makes it closable;
none was closed here, because each needs its own reading of what durable rule it
carries before it is deleted (`issues/README.md`).

| slug | area | what answers it |
|---|---|---|
| `log-flush-is-unbounded` | boot-media | the idle loop has no filesystem statement and no log condition; the kernel writes no file at all |
| `client-cpu-takes-the-log-flush` | audio | there is no affordability heuristic left to steer and no CPU takes a flush. **Its hypothesis is closed unverified** — its own last section says only a metal boot can confirm it, and deleting the mechanism makes that permanently unfalsifiable, so the metal arm is owed rather than answered |
| `pre-idle-wedge-says-nothing` | diagnostics | `Drain::Inline` puts every boot record on the wire as it is written, and `pre_idle_wedge_speaks` is the gate |
| `log-ring-flushes-one-line-behind` | kernel | a commit posts `klogd`'s wake, so the halt is refused by the doorbell and the scheduler's own invariant rather than by a log-specific pre-`hlt` condition |
| `shutdown-path-logs-never-reach-console` | kernel | `SYS_SHUTDOWN` waits, bounded, for the durability word and then drains the console inline before the power goes |
| `sink-append-error-unreachable` | boot-media | its subject is deleted. **Its finding is reactivated rather than moot**: the appender is an ordinary userland process now, so its tail page is ordinary evictable page cache and the merge-into-a-failed-read it describes is reachable from the log path again. That sentence belongs in the file cache's own doc comment, with `fat-backing-read-fails` as its stager |
| `rotation-leaves-the-newest-in-the-older-name` | boot-media | stale rather than fixed: it describes a two-generation `kernel.log`/`kernel.log.1` scheme at 4 MiB that one-file-per-boot with `_NNNN` continuations replaced. Re-check that `kernel_log_file` no longer accepts "either of the two files" before deleting |
| `the-panic-path-does-not-write-the-log` | boot-media | `kind: rejected`, and its argument is a property of the architecture now rather than a decision: the panic path writes the backend and never an object, so it depends on no daemon and no lock the dying thread might hold |
| `idle-machine-looks-wedged` | kernel | already superseded by a defect that was found and closed; and after the record ring the last line before a quiet period *does* reach the wire, so "the log stops here" is evidence rather than an artefact of the drain |
| `redesign-the-log-subsystem` | design-debt | `kind: question`, and **only its design half is answered** — the `kernel/src` layout half is untouched and is nobody's to answer but the owner's. It is not closable as it stands; it wants splitting, and the split is his call |

**Re-scoped rather than closed.** `log-is-userland-writable` (boot-media) keeps
its residuals, and its first one changes character rather than being half-done:
`/log` is written by a userland daemon on purpose now, which is the opposite
decision rather than an unfinished one.
