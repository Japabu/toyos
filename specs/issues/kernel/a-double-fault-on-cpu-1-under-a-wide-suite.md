---
status: open
kind: finding
opened: 2026-08-18
---

# A `DOUBLE FAULT` on CPU 1 killed a guest under a twelve-wide suite, and nothing kept its report

Seen once, dev host, `cargo test` twelve wide, 2026-08-18, on
`wt/toyos-purecrates` — a branch whose whole delta is three kernel files moving
into two pure crates with **no line of their logic changed** (`diff` of each
moved file against its original: doc comments, one `use` path and two test
paths, nothing else). The same tree is green on all twelve KVM shards of CI
(PR #124).

```
FAIL console_line_atomicity: kernel panic: DOUBLE FAULT on CPU 1 (pid=Some(Pid(2)) tid=Some(Tid(0))) — the guest went quiet because every CPU is halted, not because it was still working. The panic is the finding and the guard never got to be one.
stdout:
logd: this boot's kernel log is /log/2026-08-18-113110.log (2026-08-18 11:31:10 at UTC+0 recovered f
soundd: suspended
  FAIL  console_line_atomicity  (21s)
```

`ALONE console_line_atomicity: GREEN`, by the harness's own re-run. The host
line for that run: `fastest boot 1522 ms against the reference 1320 ms —
liveness ceilings paid at 1.15x width`. The same name passed in 9 s in the full
run before it and red in the full run after it on a different and already-filed
defect (`writer A declared 1000 whole lines and the capture carries 798`,
`specs/issues/build/console-line-atomicity-reds-on-a-short-capture.md`), so
what this name does under load is red in more than one way.

**This is the sighting
`specs/issues/build/every-recorded-stall-predates-the-panic-discriminator.md`
said would start arriving.** That file's closing paragraph names the dev-host
load family and says `ALONE: GREEN` is not evidence against a panic — a panic
reached only under contention does not reproduce alone either. Before
2026-08-17 this run would have been recorded as a stall. It is a kernel death,
it is named, and that is the whole of what has improved.

## What is not established, and why nothing can be read further

**The kernel's own report is not in the record.** `console_line_atomicity`'s
failure arm returns `format!("{err}\nstdout:\n{}", tail(&result.stdout))` —
`TestResult::serial` is right there and is not printed — so the two userland
lines above are everything that survived. The double-fault handler writes a
full report on IST1 and `double_fault_stack` measured it at *6688 of 16384
bytes* in this very run, so there was a report; nothing kept it.

Which process `Pid(2)` is in that boot is therefore also unestablished. It is
an early daemon of a guest booted `smp: 2` for this test, and naming it would
take the capture that was dropped.

## What to do when it happens again, in order

1. **Print `result.serial` in that arm**, and in every arm shaped like it — the
   fix is one format string and it is the difference between a finding and a
   diagnosis. `specs/issues/build/a-failure-message-drops-the-lines-before-the-test-started.md`
   is the same hole seen from the other end.
2. Boot it with `BootOptions { qmp: true, .. }` so the guest survives the
   verdict and `info registers -a` can say what the other CPU was doing
   (`specs/debugging.md`).
3. A `DOUBLE FAULT` is a fault taken while delivering a fault. The two readings
   to separate are a kernel stack that overflowed and a fault raised on the
   fault path itself; the report on IST1 says which, and it is the artefact
   step 1 recovers.
