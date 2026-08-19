---
status: expected-red
kind: defect
opened: 2026-08-06
task: 156
---

# `desktop_window_child` reproduces the machine freeze, and must stay `Sched::Parallel`

**Read this before touching that test.** It is red on `main` today, on purpose,
and the two obvious ways to make it green both destroy the only QEMU
reproduction anybody has of #156.

**The signature, precisely, because there are two different reds it can give.**
The one that means #156 is a **total freeze of the guest**: a round opens
snake, the shell echoes its prompt, and from that instant the guest emits
*nothing at all* — not `exit: snake`, not a shell line, and **not the
compositor's `compositor: frames=…` stats line, which had been arriving every
~2 s until then**. The harness drains its full ceiling and appends nothing. The
missing periodic line is the discriminator: any other failure of this test
leaves output flowing and fails an assertion with the log still filling. If you
see the test fail *with* serial output continuing, that is a different defect
and this entry does not cover it.

Independently hit 10/10 across four invocations by an agent trying to land
unrelated documentation, in the 12-wide parallel phase, with the harness's
re-run-alone pass reporting GREEN each time.

**It stays `Sched::Parallel`, and `ALONE: GREEN` here is information rather
than a misclassification.** The freeze needs contention to appear, so the
classification that looks wrong is the one that reproduces it. `Sched::Serial`
would make the suite green and take the reproduction with it. (The classifier
is not trustworthy in general — the xHCI work established that `ALONE: red
again` can measure the host rather than the tree — but in this direction, on
this test, green-alone is real.)

**A third manifestation, 2026-08-06**, on the mechanism branch, twice in one
session — once in the 12-wide phase and once in the re-run alone, at 3.5 s into
both boots. The message is `GUI+Q never reached the compositor`, **and it names
the wrong thing**: the close did reach the compositor. The log under it is the
teardown, one probe earlier than the snake rounds — `exit: test_rs_window_child
pid=5 code=0`, then `exit: shell pid=2 code=0`, then `exit: terminal pid=1
code=0`, then `windows=0`. `close_focused_window` waits for `windows=1` and the
desktop went straight to none, so the harness reports the injection it did
deliver as one that never arrived. Serial kept flowing for the whole drain —
compositor stats every ~2 s, kernel stats every 10 s — so by this entry's own
discriminator it is **not** the freeze; it is the shell-exit defect three
paragraphs down, reached at the first windowed child rather than during a snake
round. Whoever fixes that message should make it say what the log says.

**And that third manifestation is now the only one this test produces, which
means the reproduction is masked (2026-08-06, eight boots).** Two full-suite
runs in the 12-wide phase and six alone — three with the winit lock at
`be9ec72c`, three at `faf99eb7` — are red every time and **not once the
freeze**. Every capture has the guest alive for the whole drain: compositor
stats every ~2 s, kernel stats every 10 s, the i8042 counter climbing to 4818
keys as the harness re-injected GUI+Q, and all eight vCPUs `HLT=1 RFL=0x246`,
which on a settled desktop is an idle machine and not #156. What all eight show
is the teardown above, now at the **second** probe — the owner's case, a live
client whose window is taken away — with the client leaving `code=0` before the
shell does. One of the eight got through to snake round 0 and produced the same
shape with `exit: snake pid=7 code=0 cpu=1224ms` in its place, so where the test
stops varies with load and what happens does not.

Two consequences for whoever picks this up. **`ALONE: GREEN` no longer holds** —
6 of 6 alone are red — so the paragraph above it is a record of what the test
used to do, not a prediction. And **the freeze's venue is unreachable**: it was
seen in a snake round, and the desktop is torn down one or two probes before
that. Fixing the shell-exit defect is now on #156's critical path rather than
beside it. The teardown is not a regression from the deadline fix (`add6aeb`,
18:05): the paragraph above it was written at 17:32 describing the same three
exits, and is not a descendant of it.

**CLOSED, and it was the harness closing the desktop twice (2026-08-06).** The
teardown in the three paragraphs above is not a guest defect at all.
`close_focused_window` looped on `log[new..]` but waited with `serial_until`,
which scans the whole capture, so the *previous* probe's `windows=1` answered
the wait instantly and it re-sent GUI+Q at the speed of a QMP round trip. The
second one closed the window under the one it meant. The compositor now says so
itself and the two closes are one line each:

```
compositor: window closed pid=5 by GUI+Q, 1 left
exit: test_rs_window_child pid=5 code=0
compositor: window closed pid=1 by GUI+Q, 0 left     <- the terminal
exit: shell pid=2 code=60
exit: terminal pid=1 code=0
```

Everything after the second close is correct: the terminal breaks on
`Event::Close`, drops `shell_stdin`, the shell's stdin reaches genuine EOF
(`60` is `UnexpectedEof`, encoded into the exit status because a diagnostic
through that pipe is lost with it), the shell leaves and the terminal reaps it.
So the shell-exit reading below — "the failure is the read after the prompt" —
was right about the mechanism and wrong about the cause: nothing was wrong with
the read, its writer had been told to go.

The fix is `note_closed` in the compositor plus a harness that waits on that
event instead of sampling `windows=N` every two seconds — recorded in the
scheduler migration log, now git history rather than a live file.
**`desktop_window_child` now passes end to end,
alone and in the 12-wide phase** — both windowed-child probes and all three
snake rounds, snake leaving `code=0` each time.

**What this does *not* settle is the freeze**, and the entry stays. It stays
`Sched::Parallel` for the same reason as before, `EXPECTED_FAILURES` keeps its
declaration to its review date, and a green run still proves nothing — the
signature at the top of this entry is a guest that goes *silent*, and none of
the eleven boots in this session produced one. What has changed is that the
test can now reach the snake rounds where the freeze was seen, which it could
not before. Judge the next occurrence by the signature, never by a run.

**Landing while it is red** needs nothing special: `desktop_window_child` is
declared in `EXPECTED_FAILURES` (`tests/toyos.rs`) and the gate is the ordinary
one. The declaration reports it by name on every run, is red
if the test *passes* where the entry says a pass is proof, and is red on
`2026-09-06` regardless — this entry is intermittent, so its own expiry is a
date rather than a green run. The `--skip` flag that used to be the answer is
deleted: an exclusion nobody reviews cannot expire, and this one has to.

**CLOSED — a stale `--skip` command line was worse than a refused one, measured
2026-08-06.** The flag was gone but the words still parsed, and `--land --gate
cargo test -- --skip desktop_window_child` — the form CLAUDE.md and every
handover carried until that week — reached the harness as a *filter*.
It ran exactly one test, `desktop_window_child`, declared it expected, and
landed on that. `--land` does print `the gate was NOT the default cargo test`,
which is the only thing that saved it; the run itself looks like a pass.

The asymmetry that entry named — a stale feature name refused against
`kernel/Cargo.toml` before any lock, a stale gate flag not refused at all — is
closed. `toyos_build::testargs::parse` holds one table of the flags the suite
has, refuses anything else by name and by consequence, and returns the filter
out of the same pass, so no word can be a flag's value to one reader and the
filter to another. It is the first thing `main` does, before the sysroot lock
and before anything is compiled. A flag added to the harness and not to that
table is refused the first time it is typed, which is the direction of drift
that says so rather than the one that narrows a gate.

**What the declaration will and will not absorb.** Its `says` list covers the
six of this test's messages whose failure is *the desktop ceasing to answer
after a window closed*. The other five red the run — the client binary missing,
the desktop never coming up, a window never being created, and the client
leaving on its own deadline. That pins which assertion failed and not why, so
the log-tail discriminator above is still a human's to apply; the run prints the
pointer to this section beside every `XFAIL` line for exactly that reason.

**One thing #156's capture leaned on is closed, and it is not this.** The
deadline was stored twice — `ParkedEntry.deadline` and `DeadlineHeap` — and
`fire_deadlines`' lost claim discarded one copy, so a CPU could halt with
`TimerPlan::Stop` while its report said `1 pending, 0 OVERDUE`. That is why the
dump taken off the frozen guest could not be read, and it is fixed (recorded
in the scheduler migration log 2026-08-06, now git history): a deadline lives
in one place and
invariant T reads what arms the timer. **This entry stays open.** Nothing
established that the divergence is what froze the guest — the claim's
`Msg::Wake` follows it within two instructions and `SleepArm::confirm` refuses
to halt on a non-empty mailbox — and a green run of this test after that change
is the race landing the other way rather than evidence. Judge it by the
signature above, never by one run.

**What the test was built to chase is still open underneath it**, and is not
the same thing: on `boot8-snake.log` the owner closed snake's window and got
`exit: snake pid=10 code=0` at 121.659, `exit: shell pid=5 code=0` at 121.693,
`exit: terminal pid=4 code=0` at 121.706. A shell that has reaped its
foreground child prints a prompt; it does not exit. The harness has now shown
the same three-process teardown after a window close — child `code=0`, then the
shell, then the terminal — with a bare `window::Window` client and no winit
anywhere, so it is not snake's and not the fork's.

**Which of the two readings the evidence supports.** In the harness run the
shell's prompt *is* on the serial log, between the child's exit and its own,
mirrored by a terminal still inside its loop. So the shell reached its prompt
and then went: the failure is the read *after* the prompt, not the prompt. That
is "the shell exits instead of prompting" rather than "the chain is torn down
child-first" — and it points at the shell's stdin, whose only writer is a
terminal that was demonstrably still running. On the T14 the same window shows
no prompt at all, which is the weaker evidence of the two and may simply be a
line that never got flushed.

**A confound that is not this.** Three of the owner's eight CPUs stop taking
scheduler passes during a session and threads placed on them never run (`issues/kernel/`,
#142/#156). That produces processes which *hang*; the three here *exited*,
promptly, with `code=0`. This entry must not be closed by it — though the
freeze the test now reproduces is very likely that defect, which is the whole
reason the test is worth keeping red.
