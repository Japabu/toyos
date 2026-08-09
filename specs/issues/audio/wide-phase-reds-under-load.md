---
status: open
kind: finding
opened: 2026-08-07
---

# The wide phase still reds under host load, and the shootdown fix's effect on the rate is unmeasured

**The signature's cause is closed: two CPUs shooting down at once**
(`kernel/src/shootdown.rs`, gated by `an_initiator_answers_while_it_waits`). It
was a mutual wait and not a bound, so no deadline value was ever going to fix it; the
wait now answers before it asks. What that does *not* do is make `cargo test`
reliably green, and this section stays open for the part it does not reach.

**The wide phase still reds, on a different class, and this signature did not
reproduce here at all.** Measured 2026-08-07 on `wt/toyos-tlbfix`, four full
suites and 96 guest boots of the audio family, **zero `tlb:` lines in any of
them** — including 50 boots on a kernel with the fix reverted, where the H3
agent's twelve-run hunt had found it in roughly one boot in five. So the rate
this defect ran at earlier in the day is not the rate it runs at now, and no
measurement taken here can be read as the fix having lowered it. The fix rests
on the shootdown's own two backtraces and on
`an_initiator_answers_while_it_waits`, which is red without it.

| run | wall | verdict |
|---|---|---|
| before the fix | 576.3 s | 4 red: `metal_sim_compositor_stall`, `metal_sim_client_death`, `screen_blocked_dump` (all `ALONE: GREEN`), `audio_tone (smp=8)` |
| after, 1 | 559.3 s | 2 red: `i8042_mouse`, `desktop_audio_client` — 385 s wide against 13 s alone — both `ALONE: GREEN` |
| after, 2 | 182.7 s | **clean, 289/289**, on a host that was briefly quiet |
| after, 3 | 704.7 s | 1 red: `screen_blocked_dump`, `ALONE: red again` |

Every one of those reds is the parallel-red list in
`specs/issues/build/parallel-tests-red-under-other-suites.md`, not this entry: the
two that name a duration are the contention class, and the one clean run is the one
whose host was idle. **A landing is still a coin toss and the reason is now
squarely that list**, whose own last paragraph says a verdict that flips with
the host is measuring the host. `audio_tone (smp=8)`'s `suspend structure: no
'soundd: suspended' after the last client removal` fired 2 of 12 on the reverted
kernel and 0 of 12 on the fixed one, which at n=12 is not a difference and has
no mechanism behind it — recorded so the next person does not read it as one.

What follows is the evidence as it was recorded on `wt/toyos-boot`, and it is
what pointed at the shootdown.

Measured 2026-08-07 across two `--land` gates on `wt/toyos-boot` (289 tests
each) and five A/B runs against `main` at `6d11938`, all in one session.

Seven distinct tests failed between the two gates —
`null_sink_shipped_client`, `metal_sim_window_caps`,
`metal_sim_ipc_hostile_peer`, `metal_sim_compositor_stall`,
`metal_sim_client_death`, `desktop_window_child`, and an `hda_tone` that is
`hda-tone-phase-check`. **Every one of their captures carries the same two lines**, with
different generation numbers:

```
tlb: cpu 1 has not flushed for generation Generation(69) in 5000000000ns
     — it is not taking interrupts
tlb: cpu 0 has not flushed for generation Generation(68) in 5000000000ns
     — it is not taking interrupts
```

And every one of them **passes alone, on both trees**:

| test | in the wide phase | alone on the branch | alone on `main` |
|---|---|---|---|
| `null_sink_shipped_client` | FAIL, 10 s | PASS, 4 s | PASS, 5 s |
| `metal_sim_window_caps` | FAIL, 5 s (three times) | PASS, 3 s | PASS, 36 s |

`metal_sim_window_caps` is the clearest: its own work *completes* —
`window caps: oversized refused, 62 windows granted then refused` — and the
process then exits `-1` after two CPUs have each stalled five seconds. 62
windows created and destroyed is 62 rounds of unmapping, which is exactly what
`arch::tlb::shootdown` is on the path of. `null_sink_shipped_client`'s round 1
took 6.14 s for a 3 s tone and its round 2 then panicked in
`toybox/src/tone.rs:85` on `failed to open audio stream: NotFound`.

So the branch is not the variable. The shootdown wait landed on `main` the same
day (`318ec10`, `c4173f0`) and **its own diagnostic is what named the stall**, so
the instrument is already in the tree.

The reading that "the load is the variable" was the wrong half of it, and worth
keeping as the mistake it was: load is what made two unmaps overlap, and the
overlap was fatal by construction. The generations here differ by one — `68` and
`69` on two CPUs of a two-CPU guest — which is the same pair of initiators the
shootdown's backtraces name.

**The load is the variable, not the width.** Four full runs, same session, same
289 tests, one branch:

| width | host | verdict |
|---|---|---|
| 12 (default) | this worktree alone | 2 failed, 287 passed — 526.9 s |
| 12 (default) | this worktree alone | 5 failed, 284 passed — 497.0 s |
| **4** | this worktree alone | **289 passed, 0 failed — 265.2 s** |
| 4 | `toyos-tlbfix` running its own suite | 4 failed, 284 passed — **610.4 s** |

The third row is the one that looks like a fix and is not: the fourth is the
same width against a second worktree's suite, and it is red again with three of
the same victims (`metal_sim_window_caps`, `metal_sim_ipc_hostile_peer`,
`metal_sim_compositor_stall`) and the same `tlb:` lines. What the third row does
show is that **4-wide beat 12-wide on wall clock by a factor of two on a quiet
host** — `specs/test-cost-audit.md` §4.1 constraint 3 arriving from a new
direction, and a measurement worth re-taking on its own terms.

So `--land --gate cargo test --test toyos-build -- --jobs 4` is a way through
when the host is otherwise idle, the landing prints it as an override, and it
fixes nothing.

**Not to be re-run away**: the owner's 2026-08-04 ruling is that a
load-coincident failure is a real defect, and this one reproduced across two
full runs with seven different victims. That ruling is what produced this entry
instead of seven re-runs, and it is what the fix came out of.
