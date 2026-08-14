---
status: open
kind: defect
opened: 2026-08-01
---

# No test distinguishes the crash-report preemption fix from a no-op

`bd12795` rests on reading the code, which is the weakest standard this project
accepts. Staging it needs a crash report whose preempt count returns to zero with
`need_resched` set — a timing coincidence the harness cannot ask for. The three
panic-path tests still passing says only that nothing regressed.

Fourth instance this session of the "break it and run it" pattern in
`specs/README.md`'s spec-checking method, and the only one of the
four where the check is genuinely hard rather than merely skipped. Recorded so it
is not mistaken for the same tested standard as the fixes around it.
