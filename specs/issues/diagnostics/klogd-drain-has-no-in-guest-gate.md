---
status: open
kind: finding
opened: 2026-08-14
---

# `klogd`'s drain has no in-guest gate, and Ctrl+Alt+D cannot be one on a headless guest

The log architecture gives the machine a kernel thread, `klogd`,
made runnable at the commit of the record it will drain. The *ordering*
of that wake is modelled — `kernel-loom/tests/log_wake.rs`, with its
`wake-fence-off` negative control — and the thread's *hosting* is gated by
`klogd_hosted`. **What no test asserts is that the wake fires in a guest**: that
a booted machine's records actually reach `klogd`.

The number exists. `sched::dump` prints it, and on the dev host a
`tests/testcases` boot reports

```
== klogd: 143 record(s) drained, 0 lost, 6 park(s)
```

— the whole boot's records, drained across six parks, which is the wake firing
six times. Nothing asserts it.

## Why the obvious gate does not work

Pressing Ctrl+Alt+D over QMP from `klogd_hosted` was written, measured and
reverted. The `Headless` profile carries an i8042 whose pin never asserts — its
own verdict line says so at ~240 ms — and QMP's `input-send-event` qcode lands
on that controller rather than on a keyboard the guest is listening to. The
result is intermittent by construction: green alone twice, then
`STALLED: waiting for the whole dump — it went quiet` in a full suite *and* in
that suite's own ALONE retry. A gate whose verdict depends on which input device
QEMU picks is not a gate.

## What would work

Either a profile whose keyboard is real — `blocked_dump` presses the same chord
on `Profile::Metal` and already holds the report, so one assertion there costs
nothing but adds it to a name `--known-red` already tracks — or L4's
`SYS_LOG_READ`, at which point `test-runner` reads the records itself and
§9.1's `log_conservation` subsumes this entirely.

**L4 is the answer and this entry closes with it.** It is filed rather than
fixed because L3 must not grow a gate that L4 deletes.
