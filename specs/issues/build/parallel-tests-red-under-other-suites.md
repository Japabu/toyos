---
status: open
kind: defect
opened: 2026-08-04
---

# `Sched::Parallel` tests that go red under other worktrees' suites

Caught by the re-run-alone pass (`specs/test-cost-audit.md` §5.4.6) on
2026-08-04, on a host carrying three to four concurrent full suites, and green
the moment each was re-run by itself in the same process. None predates or was
introduced by the parallel-width work; all have been `Sched::Parallel` since the
phase landed, and none reproduces on a host running one suite.

**Read this list against `specs/test-cost-audit.md` §5.8 before adding to it.**
Every entry below that says `nothing typed at the terminal window reached a
shell` — `desktop_typing_damage`, `desktop_locale_detect`, `blocked_dump`, and
`desktop_audio_client` in `desktop-window-child-holds-a-lane` — is now known to be the
`/bin/terminal` boot race (`kernel/terminal-races-compositor-at-boot`, since
closed by the capability endowment branch: a port exists before either end's
process does) reported through a wall-clock guard that could
say nothing else: three of three such reds in an eight-suite session carried the
race in their boot log, and the wait they blew had been ruled out at 0.6 s by
`exit: terminal pid=N code=1`. `shell_echoes` names the race now. So an
`ALONE: GREEN` beside one of those sentences was never evidence that the host
was the cause; it was evidence that the *boot* differed, which a re-run also
changes.

- **`i8042_mouse`** — CLOSED 2026-08-06. Both red modes were the harness and
  neither was ever the driver losing a packet; `specs/issues/hardware/`'s entry carries the mechanism,
  the measurements and the two gates that now hold each half. The short version:
  the pacing lead was 32 packets — 96 bytes — against QEMU's 16-byte
  `PS2_QUEUE_SIZE`, so a host that got ahead of the guest made QEMU *sum* the
  motion it had no room to queue, and a summed pair that cancels reaches
  userland as nothing at all. The lead is now 4 packets with a `const` assert
  against the device's queue, and the lost-edge counter no longer fires on a
  pass that read the `irq_ring` record a few instructions before the ISR
  published it.
  **Not closed after all, at the count.** 2026-08-07, two full suites in one
  worktree while a second worktree held six of the twelve guest slots: `1003
  pointer events reached userland out of 1004 packets injected, never more than
  4 of them (12 bytes) outstanding against a 16-byte device queue`. The lead is
  inside the bound the fix installed, so the summing mechanism `specs/issues/hardware/` describes is
  not what this is. A/B in one session, `git checkout main -- kernel/` in the
  same tree minutes apart: this branch PASS 33 s first try, **main's kernel FAIL
  with the identical 1003-of-1004 line**, then PASS 2 s on the harness's own
  re-run. So it is not a tree difference and it is not gone — one packet in a
  thousand is still being lost, or still being counted wrong, under a host
  carrying two suites.
- **`i8042_absent`** — same session, same shape, and it is `Sched::Serial`
  already, so intra-suite width is not what reaches it. The verdict is the
  guest's own `Boot: complete` on two boots with a 300 ms allowance; the landing
  gate saw `601ms without an i8042 and 287ms with one`. Alone, minutes later:
  this branch 619 vs 507 (PASS), main's kernel in the same tree 277 vs 331
  (PASS). The absolute figure moved 277→619 ms across three runs of one boot
  with no code change, so what the allowance is being asked to absorb is the
  host, and a serial slot inside one suite does not buy a quiet one.
- **`usb_transport_break`** — now `Sched::Serial`. The cause written here was
  wrong and the correction is in `specs/issues/hardware/`, *a Bulk-Only Reset that raced the transfer
  it was recovering from*: the second line is the **device** stalling the next
  command block, on an endpoint the recovery found Running and not halted, and
  it was a driver defect that lost the caller's write rather than a count of how
  much of the host the guest had. Closed.
- **`desktop_typing_damage`** — `nothing typed at the terminal window reached a
  shell`. `shell_answers` typed ten times with a flat two seconds between, which
  is a twenty-second ceiling on a desktop coming up; the retry window is now
  `qemu::budget(20 s)`, the phase's. Still `Sched::Parallel`. **See
  `desktop-window-child-holds-a-lane`: as of 2026-08-06 this is no longer
  occasional but reproducible, and the mechanism is the duration profile.**
- **`desktop_locale_detect`** — added 2026-08-05. Same `nothing typed at the
  terminal window reached a shell`, same `ALONE … GREEN`, in the same run as the
  entry above and on a branch that touches neither the compositor nor the
  terminal. It reaches a shell through `shell_answers` exactly as
  `desktop_typing_damage` does, so it inherits that retry window and evidently
  not enough of it. Still `Sched::Parallel`.
- **`netd_connection_caps`** — added 2026-08-05. Red at 50 s inside a landing
  gate that was otherwise 257/259 with 0 invalidated, green in 7 s alone on the
  same tree moments later, on a branch that touches neither netd nor the
  network stack. The 50 s against a 7 s solo run is the shape of a boot that
  never got enough of the host, not of a cap that was announced wrong. Still
  `Sched::Parallel`.
- **`metal_sim_pointer_churn`** — observed once, on a host carrying three other
  suites *and* a `toyos-sched-sim` run. Not investigated. Still
  `Sched::Parallel`.
- **`dump_nmi_probe`** — added 2026-08-07, and the odd one out: it is already
  `Sched::Serial`, so it failed in the *serial tail* rather than the wide phase
  and the harness therefore never re-ran it alone. Run alone on the same tree
  moments later it passes in 23 s. `the NMI went unanswered too` is its
  wall-clock verdict expiring on a host carrying three other worktrees' suites —
  the `[host-slots]` lines in that run name all three. `4ad8875` made it serial
  for exactly this reason, which shows what serialising buys and what it does
  not: within one run the phase is quiet, across runs nothing but
  `buildlock::guest_slot` spans worktrees and twelve slots is not one guest.
  Nothing here should widen its millisecond.
- **`blocked_dump`** — added 2026-08-07, `nothing typed at the terminal window
  reached a shell`, `ALONE … GREEN` in 5 s. Same shape and same sentence as
  `desktop_typing_damage` and `desktop_locale_detect`: its verdict is the dump's
  content, but *reaching* the dump crosses a compositor, a terminal and a shell,
  and that step is a wall-clock margin. Still `Sched::Parallel`.
- **`screen_console_scroll`** — added 2026-08-07. `round 1: the guest never
  printed CHURN-DONE 0 100`, **598 s** in the wide phase, `ALONE … GREEN`. The
  landing gate it killed ran 778.9 s with four other `--land` processes on the
  host, on a branch whose whole delta was two documentation lines. 598 s against
  a phase that is ~45 s on a quiet host is the finding; the message is not.
  Still `Sched::Parallel`.
- **`hda_tone`** — added 2026-08-07, hours after the test itself landed. In a
  full run on a host carrying another worktree's suite: `2 mid-tone silences in
  the capture: total 2 [3p×1 4p×1]`, `dither 3.3%`, `phase-breaks 92`. Alone on
  the same tree eight minutes later: `gaps none`, `phase-breaks 16` — the
  declared #88 failure and nothing else. It is `Sched::Serial`, so like
  `dump_nmi_probe` the harness never re-runs it alone and the run simply reds.
  Its `EXPECTED_FAILURES` entry covers the phase-break message alone, which is
  why a *dropout* under load reaches the verdict, and that is correct: **do not
  widen it.** A silence and a phase break are two different defects and an entry
  that covered both would stop saying anything. The tree it was seen on differed
  from main only in `src/`, so the guest image was byte-identical to main's.
  **Three times the same day**, all three in landing gates of that one
  build-system branch and all three confirmed alone within ten minutes: `2
  mid-tone silences`, then `1 mid-tone silence`, `gaps none` alone every time,
  with three to five other `--land` processes on the host. Ask
  `git diff main...HEAD` and never `git diff main` when checking whether a tree
  could be the cause: the second is symmetric and lists what *main* changed since
  the branch last merged, which reads as the branch's own work and is not.

- **`xhci_hid_break`** — added 2026-08-07, in a landing gate on a branch whose
  delta since its own previous green gate was one documentation commit. `input
  never came back: no pointer event moved by (2560, -1920); deltas seen:
  [(256, 256), (256, 256)]`, `ALONE … GREEN`. The two deltas it did see are the
  boot-time absolute tablet, so what went missing is the relative mouse's event
  after the staged break — a wall-clock margin on the recovery path, not a
  recovery that failed. It is one of the three longest jobs in the suite by
  `longest_first`'s own profile, so it is dispatched early and runs beside
  everything. Still `Sched::Parallel`.

**The eight-landing regime, and what it does to the paragraph above.** That
paragraph says the four-suite regime "cannot recur" now that `guest_slot` admits
twelve guests across every worktree. It recurred on 2026-08-07: **eight
`toyos-build --land` processes were queued on the integration lock at once**, and
one branch's two consecutive landing gates died on two *different* tests from
this list — `blocked_dump`, then `screen_early_panic` — each `ALONE … GREEN`,
neither related to a branch that touched only `tests/`. The semaphore is not
wrong; it counts the thing it says it counts. But a landing gate is a full build
plus a suite, and **the build half is bounded by nothing** — eight of them is
eight cargo trees compiling on 14 cores, which reaches every liveness margin in
the wide phase without a thirteenth guest ever existing. The gate's own audio
lines recorded the host at seven `toyos-build` processes throughout.

So the closing claim needs the qualifier: guest slots bound *guests*, and a
landing storm is not made of guests. Whether the integration lock should also
gate the gate's build, or whether these tests belong in the serial tail, is a
decision for whoever owns the harness; what is established here is that a branch
can be unable to land for reasons that have nothing to do with it.

**Bounded the same day, and the count was closer to home than a landing storm.**
A worker takes a guest slot and then *compiles its kernel variant*, so twelve
workers in one suite are twelve concurrent `cargo build`s before any of them
boots — which is the load 49.9 with twelve rustc/cargo processes and exactly one
guest live that was measured while this was being written, on a host where the
semaphore was doing precisely what it says. `buildlock::build_slot` is the
second count: four across every worktree, its own directory so a suite holding
every guest slot can still compile, `--host-builds N` to override and `0` to turn
off (`specs/test-cost-audit.md` §5.7). It bounds the build half of a landing gate
by construction, since a gate's builds are these builds. What it does **not**
bound is anything that never enters `src/build.rs` — a `toyos-sched-sim measure`,
a hand-run `cargo build` in a fork clone, the primary's `./x.py`.

**What to do about a red on any of these names:** read the `ALONE` line under it
before anything else. `GREEN` there means the host, not the kernel. What none of
them should get is a widened bound — a gate that tolerates one lost byte
tolerates the defect it was written for. The two fixes above are the two shapes
that are legitimate: make the verdict independent of the rate, or scale a
liveness ceiling with the phase. The global QEMU-slot semaphore this section
used to name as the closing move now exists (`buildlock::guest_slot`,
`specs/test-cost-audit.md` §5.6): the host admits twelve guests across every
worktree, so the four-suite regime these were observed in cannot recur. A looser
assertion is still not the answer.

**But `ALONE … red again — the defect is real` is not evidence, and the protocol
above leans on it.** The re-run happens inside the same process, moments after
twelve guests have been torn down and while another worktree's suite may still
own the host — so it is alone in the suite's bookkeeping and not on the machine.
Measured 2026-08-06 on the xHCI port-machine branch, whose kernel delta is
`drivers/xhci/` and touches no PS/2 and no compositor path:

```
full suite, run 1 (483.7 s for 262 tests):
  FAIL i8042_mouse — 975 of 1004;  ALONE: GREEN
  FAIL screen_early_panic;         ALONE: GREEN
full suite, run 2, the landing gate (512.1 s):
  FAIL i8042_mouse — 560 of 592;   ALONE: red again — the defect is real
  FAIL desktop_locale_detect;      ALONE: red again — the defect is real
then, genuinely alone, same session, minutes later:
  main         a051a67:  i8042_mouse PASS 10.4 s   desktop_locale_detect PASS 11.4 s
  the branch   38431c7:  i8042_mouse PASS  4.1 s   desktop_locale_detect PASS  5.6 s
```

Both trees green on both tests with the host to themselves, and the same suite
that took 120.4 s at the last quiet landing took 484 and 512 s in these two — so
the host was carrying roughly four times its own load throughout, the `ALONE`
re-run included. A verdict that flips between "GREEN, it is the host" and "red
again, the defect is real" for one test on one tree twenty minutes apart is
measuring the host in both directions.

Consequence for the protocol: `ALONE: GREEN` still means what it says, because a
green cannot be produced by load. `ALONE: red again` means nothing on its own
and must be confirmed against `main` in the same session before it is believed —
which is the A/B the audio rules already require and which this line currently
invites an agent to skip.
