---
status: open
kind: finding
opened: 2026-08-18
---

# Two independent guests disconnected within seconds of each other in one 12-wide phase

Dev host, `wt/toyos-faultclass`, 2026-08-18, one full `cargo test`
(263 passed / 2 failed / 265 total, 88.1 s), with another agent's suite holding
guest slots throughout — the run's own `[host-slots]` lines name two of that
agent's tasks.

```
FAIL i8042_kbd_echo: QEMU disconnected
FAIL rs::va_exhaustion: QEMU disconnected
--- the kernel said nothing while it ran ---
FAIL va_exhaustion: the guest did not survive exhaustion:
```

Both are `MACHINE_TESTS` with a boot each, so these are **two separate QEMU
processes**, and both went in the same parallel phase, 57 log lines apart. Both
were `ALONE: GREEN` in the same run, both green in a second full run four
minutes later on the same tree, and neither is on the redlist
(`cargo run -- --known-red` answers `NOT ON THE LIST` for each).

**What makes it a file rather than a line in somebody's summary:** neither guest
said anything. `QEMU disconnected` is the reader thread reaching EOF — the QEMU
*process* exited — and `va_exhaustion` reports `the kernel said nothing while it
ran` beside it. A guest that panics or halts leaves a report; these left
nothing, which is what a killed process leaves. Two of them at once is a claim
about the host and not about either test.

`va_exhaustion`'s half is already recorded, with this exact sentence pair, in
`specs/issues/build/free-memory-verdicts-share-a-boot.md` — as a *verdict* about
how much memory the machine has, taken while other guests compete for it. What
that entry does not carry is a second, unrelated guest dying in the same window,
which is why this is filed apart: if the cause is the host reclaiming memory
from QEMU, the memory-verdict framing is one symptom of it rather than the
subject.

`i8042_kbd_echo` has no recorded sighting of any kind. This is its first.

Not investigated. The class is
`specs/issues/build/parallel-tests-red-under-other-suites.md`; nothing here
adjudicates anything in it.
