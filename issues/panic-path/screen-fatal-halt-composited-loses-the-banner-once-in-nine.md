---
status: open
kind: finding
opened: 2026-08-22
---

# `screen_fatal_halt_composited` writes the marker without the panic banner, once in nine

```
FAIL screen_fatal_halt_composited: /log/2026-08-22-134232.log carries the marker
without the panic banner, so what reached the stick is not the report
```

Measured 2026-08-22 on the dev host, `cargo test --test toyos-build -- --nightly
screen_fatal_halt_composited`, nine runs in one session: **one failure, eight
passes**, the failure first and the eight consecutive after it. The host was
running three agents' suites at the time and the failing run is the one that
overlapped another agent's twelve-wide phase; the eight were taken as that
drained. `cargo run -- --known-red screen_fatal_halt_composited` answers
`NOT ON THE LIST` — nobody has ever written a rate for it, which is why this
file exists rather than a re-run.

**What the message is about.** The file reaching `/log` at all means `/bin/logd`
ran and its `fsync` returned; what is missing is the report's own lines. That
puts it on `apic::wait_for_log_file`'s side of the panic path — `LOG_FILE_DRAIN`
is half a second for a dying machine to have logd scheduled, read the ring,
write the FAT append and flush the device, and the doc comment there argues the
number as policy rather than as a prediction. A machine whose CPUs are being
timesliced against three other guests is exactly the case that budget does not
cover, and the expiry is a *degraded answer* (`Budget`), not an assertion — so
the test's verdict and the kernel's own bound disagree about whether a slow
drain is a failure.

**Not the diff it was found under.** It surfaced on the branch that made six
`arch::cpu` wrappers `unsafe fn` and deleted the `#DB` handler. The first half
emitted no different instruction anywhere — `wrmsr` 51, `outb` 148, `outw` 1,
`inb` 143 in the whole kernel `.s` before and after, at the tree's profile — and
the second half is one `IA32_FMASK` bit and a handler nothing on the panic path
reaches. Neither touches `wait_for_log_file`, `logd`, the FAT append or the
flush.

**What would settle it** is a rate on `main` in one session against a rate on a
quiet host: either the number is the same, in which case the bound and the
verdict need reconciling (a `Budget` that expires is not a red), or it is not,
and something in the panic path is slower than it was. Nine runs cannot tell —
one failure gives no power at all.
