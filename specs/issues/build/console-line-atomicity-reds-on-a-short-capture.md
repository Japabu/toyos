---
status: open
kind: defect
opened: 2026-08-15
---

# `console_line_atomicity` can red for a capture that lost lines, which reads as the buffer breaking

Observed once on 2026-08-15, dev host, twelve wide, with a second worktree's
suites and a CI cycle up:

```
console_line_atomicity: writer B declared 1000 whole lines and the capture carries 874
```

`ALONE: GREEN`, and five re-runs immediately afterwards were green with `0
mixed` every time — so nothing about line atomicity moved. This is the
[`parallel-tests-red-under-other-suites.md`] class by its shape, and it is filed
apart from that list for one reason: **of every name on it, this is the one
whose red is most likely to be misread.**

The test has four assertions and only the first two are about the mechanism:
lines carrying both writers' bytes, a kernel record inside a userland line, the
run of unterminated bytes an exit flushed, and — this one — a *non-vacuity*
count, which exists so that a capture which lost the writers' output entirely
cannot report "zero mixed lines" and prove nothing. That guard did its job here.
But the sentence a reader sees names the writer and a line count, and the test's
name says "atomicity", so the obvious reading is that the per-holder line buffer
dropped 126 lines. It did not: a dropped line inside the buffer would show as
`short` (a writer's bytes at the wrong width) or as `mixed`, and both were zero.

What is unresolved is where the 126 lines went. The capture is 2000 lines of 200
bytes through virtio-console in a fraction of a second, and the host reader is a
`BufReader::lines()` on QEMU's stdout; a host that cannot keep up is the
hypothesis, not a measurement. Nobody has instrumented which side lost them.

Worth doing when somebody picks this up, in order:

1. **Distinguish the two.** The count assertion should say *what kind* of loss
   it saw — a gap in the writers' own sequence versus a short tail — so the next
   reader is not asked to infer it from a number. The guest declares its run
   already; it could number its lines.
2. Only then ask whether the loss is the host reader, the virtio-console ring,
   or the guest.

Until that exists, a red on this name is read as follows: **`0 mixed` in the
message means the mechanism held**, whatever else the sentence says.

Found while closing task #84, which rests on this gate — the premise that every
console line is one writer's whole line. That premise is not in doubt: 0 of 2000
mixed on the tree at `e064a96` and 0 of 2000 on five consecutive runs after the
red above.

[`parallel-tests-red-under-other-suites.md`]: parallel-tests-red-under-other-suites.md
