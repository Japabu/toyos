---
status: open
kind: defect
opened: 2026-07-31
---

# `SYS_PROCESS_STATS` reaches any process a handle names, and still cannot see a daemon

**The addressing half is closed and the accounting half is not.** The call takes
a `Process` handle now: a live process is sampled from its own `ProcessData`, an
exited one from the object, reading spends nothing, and two reads of a finished
process give the same numbers. `ProcessData::child_stats` is deleted, so
"exactly one question, asked once, by the parent, after the child died" is gone.
`process_stats` is the gate and `process_lifecycle` the surrounding shape.

What is left is who can ask. A handle is the whole of the right, and nothing
hands a diagnostic tool a handle to a daemon: `/bin/init` holds the only
`Process` handles for what `[boot] start` names and the only `SysCap` carrying
`Rights::MANAGE`, which is the one thing `SYS_PROCESS_OPEN` takes. So "where is
soundd's / the compositor's / netd's time going?" is still unanswerable from a
shell — `audio_idle_suspend` still name-matches `SYS_SYSINFO` entries out of a
byte buffer to sample a running daemon twice.

That is now a *policy* gap rather than an ABI one, and it has an obvious shape:
init serves a diagnostic port that answers a program name with a `Process`
handle narrowed to `Rights::READ`. `specs/plans/introspection-plan.md` is where that
belongs; nothing in the kernel has to change for it.
