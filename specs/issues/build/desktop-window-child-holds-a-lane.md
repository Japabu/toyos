---
status: open
kind: defect
opened: 2026-08-06
---

# `desktop_window_child` holds a lane for four minutes, and whichever other desktop lands beside it loses its typing window

Measured 2026-08-06 in one worktree, seven full runs, on a host at roughly three
times its own load.

Three tests boot `tests/desktopcase` at `smp: 8` — `desktop_typing_damage`,
`desktop_audio_client`, `desktop_window_child` — and each reaches its shell
through `shell_answers`, whose retry window is `qemu::budget(20 s)`.
`desktop_window_child` is an expected failure (`specs/issues/kernel/`) that runs its
`close_focused_window` retry loop out to that same budget, so it occupies one
lane for **~250 s of every run**. `longest_first` orders the parallel phase on
`target/test-durations` and therefore dispatches it first; whichever of the
other two the profile ranks next goes in beside it, waits behind two eight-CPU
guests on a fourteen-core host, and reports `nothing typed at the terminal
window reached a shell`.

| run | profile | victim | wide | alone |
|---:|---|---|---:|---:|
| 1 | none (fresh worktree) | — | — | — |
| 2 | written by run 1 | `desktop_typing_damage` | 243 s FAIL | 16 s GREEN |
| 3 | " | `desktop_typing_damage` | 255 s FAIL | 16 s GREEN |
| 4 | " | `desktop_typing_damage` | 246 s FAIL | 16 s GREEN |
| 5 | deleted before this run | — | — | — |
| 6 | written by run 5 | — | — | — |
| 7 | written by run 6 | `desktop_audio_client` | 248 s FAIL | 14 s GREEN |

**The profile is a feedback loop, and it is bistable rather than a one-way
latch.** What run 2 recorded for `desktop_typing_damage` is 243 s of mostly
*waiting for the host*, and that number is what put it back beside
`desktop_window_child` in runs 3 and 4: a duration profile whose entries include
contention cannot order its way out of the contention it measured. It releases
the same way it engages — run 5 had no profile at all, measured 17 s, and run 6
read that and left the two apart. Run 7 then promoted the *other* desktop into
the same slot, which is what says the victim is positional and not a property of
any one test.

Deleting `target/test-durations` unsticks a worktree that is in the red state.
**That is a diagnosis, not a fix**: it does not stop the next run promoting
another desktop into the slot, and a green bought that way is a green about the
ordering rather than about the tree.

Consequences worth stating separately:

- **The four minutes buy a red nobody acts on.** `qemu::budget`'s width scaling
  is right for a liveness guard on a healthy test and wrong for one that is
  known to run out: `desktop_window_child` will spend the whole ceiling on every
  run until #156 closes.
- **Two of the three desktops' verdicts are typing windows**, which is what
  `Sched::Parallel` is not for. The bullet above says the budget was "evidently
  not enough of it"; this says what it is not enough *against*.
- **`desktop_audio_client` is not a second #156.** It fails with
  `desktop_typing_damage`'s message and `ALONE: GREEN`, which is this entry and
  not the freeze, so it does not belong in `EXPECTED_FAILURES`.

Not fixed here, and the fix is not obviously "reclassify": `Sched::Serial` for
all three desktops moves ~5 minutes into the serial tail, which
`specs/assessments/test-cost-audit.md` §5.4 spent a wave getting out of. Candidates worth
pricing: cap what a lane will spend on an `EXPECTED_FAILURES` test, exclude a
test's contention wait from what the profile records, or give the profile a
notion of how much host a task wants so two eight-CPU guests do not pair.
**Whichever it is, a landing is currently a coin toss** — three of seven runs in
one session were red on this and nothing else.

**Cost another landing on 2026-08-07**, task #133, with the same signature and a
wider margin than any row above: `desktop_audio_client` **787 s** in the wide
phase against **14 s** alone, on a host carrying four other worktrees. Its
verdict line was its own (`1 of the two overlapping clients left the mixer`)
rather than `desktop_typing_damage`'s, so the message is not the tell — the pair
of durations is.

**And the holder can be its own victim.** 2026-08-07, task #152's worktree,
whole suite 500 s against the ~109 s it is ordered for, with another worktree's
toolchain build on the host: `desktop_window_child` itself took **249 s** and
failed with `nothing typed at the terminal window reached a shell` — the
*victim's* message, from the test that is normally the one occupying the lane.
So the row above's "the victim is positional" is the weaker half of the claim:
under enough host pressure the position that loses its typing window is
whichever guest is late, and that can be the four-minute test as easily as the
one beside it. It also means **`EXPECTED_FAILURES` does not absorb it** — the
entry deliberately covers only "the desktop ceased to answer after a window
closed", and this message names a shell that never answered in the first place,
so the run reds on the very test the exemption exists for and for a reason the
exemption is right to exclude.

**And it is what is left after the TLB deadlock closed.** Four full suites on
`wt/toyos-tlbfix` on 2026-08-07, one before that fix and three after: the reds
were `metal_sim_compositor_stall`, `metal_sim_client_death`,
`screen_blocked_dump`, `i8042_mouse`, `desktop_audio_client` (385 s wide against
13 s alone) and `screen_blocked_dump` again — six of the seven `ALONE: GREEN`,
every one of them this entry. The one clean 289/289 run is the one whose suite
took **182.7 s**; the three red ones took 559, 576 and 705. That is the whole
correlation, and it says the remaining landing blocker is this section rather
than anything in the kernel.

**Two of those seven were not this entry**, and that is the caution the rest of
it now carries. `screen_blocked_dump` reds at the same ~20% with the host to
itself, and the defect was in the kernel (`specs/issues/diagnostics/`, closed 2026-08-08). `ALONE: GREEN`
on a test that is red one run in five says "this re-run was one of the four
green ones", not "the phase did it" — so the classification is evidence only
where the alone rate is known to be zero, and nothing measures that.

**Two of this entry's three named victims are closed as of 2026-08-08, and the
mechanism was not the scheduler.** Both `desktop_typing_damage` and
`desktop_audio_client` reached their shell through `shell_answers`, which
retyped `echo <nonce>` against `qemu::budget(20 s)` because nothing knew when
the terminal was up — so "how long does a desktop take to come up on the host of
the day" *was* the verdict. The terminal prints `terminal: ready` now (and
`/bin/console` already printed its own), so the coming-up half waits on the
guest's own liveness and only the keystroke round trip has a clock on it.
`close_focused_window` took the same guard and it cuts the other way there: #156
is a freeze, so the machine goes quiet and the wait ends in fifteen seconds
instead of spending up to four minutes at width 12 — which is the lane
`desktop_window_child` was holding for a quarter of every run, and the whole
mechanism of "whichever other desktop lands beside it loses its typing window".

`qemu::budget` also scales by how fast the host is now, not only by how many
guests are on it (`specs/ci-plan.md` §7.2). Neither change touches
`screen_blocked_dump`, `i8042_mouse`, `screen_console_scroll` or the rest of
`parallel-tests-red-under-other-suites`' list, whose verdicts are elsewhere — this closes the desktop family's
share of it and no more. Measured after: four desktop tests wide, 16/27/28/31 s
and 18/41/48/20 s in two runs, all green; a 291-test suite at width 12 in 478 s
with `desktop_window_child` no longer in the long tail.
