---
status: open
kind: defect
opened: 2026-08-04
---

# `Sched::Parallel` tests that go red under other worktrees' suites

Caught by the re-run-alone pass on 2026-08-04, on a host carrying three to four
concurrent full suites, and green
the moment each was re-run by itself in the same process. None predates or was
introduced by the parallel-width work; all have been `Sched::Parallel` since the
phase landed, and none reproduces on a host running one suite.

**Read this list against what a wall-clock guard can actually say before adding
to it.** These entries share one shape: a test waits a number of host seconds
for the guest to do something and, when the number expires, reports the
*content* it was going to assert — so the red's name is the workload and never
the cause. Every entry below that says `nothing typed at the terminal window reached a
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
  neither was ever the driver losing a packet; `issues/hardware/`'s entry carries the mechanism,
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
  inside the bound the fix installed, so the summing mechanism `issues/hardware/` describes is
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
  wrong and the correction is in `issues/hardware/`, *a Bulk-Only Reset that raced the transfer
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

- **`handle_kill_policy`** — added 2026-08-17, **1 of 6** full `cargo test` runs
  in one session on `wt/toyos-suitecut`, whose whole delta is four test timeouts,
  with a second worktree's suite on the host throughout. `16 more killed
  processes left more live objects behind: [("Process", 6, 7)]`, `ALONE …
  GREEN`, and green on all twelve KVM shards of the same tree (run
  `32023797195`). **Its mechanism is `handle_lifetime`'s and not the terminal
  race's**: both are shared-boot binaries whose verdict is a *machine-wide*
  census either side of a kill — free bytes there, live objects by type here —
  taken on a guest where a hundred and fifty other tests are also starting and
  reaping processes. One extra `Process` between two samples is another test's
  reap that had not landed yet, and nothing in that boot arranges for it not to
  be. Still `Sched::Parallel`.

  **It has since fired on CI, byte-identical, which is what this bullet said it
  had not done.** Run `32047352064`, job `95438242676` (`guest (2)`),
  `wt/toyos-invariantp` at its merge of `origin/main`, 2026-08-17: the same
  `[("Process", 6, 7)]`, the same `ALONE … GREEN`, shard 2's other twelve names
  passing. The sentence above recording it "green on all twelve KVM shards"
  stays true of the tree it was taken on and is no longer true of the name.

  **That cuts against half of this bullet's explanation and leaves the other
  half standing.** A CI shard is one guest per machine with `--jobs 1`, so
  "another worktree's suite on the host" cannot be what did it there. What
  survives, and is the half that was always the load-bearing one, is the
  *shared boot*: the census is machine-wide and a shard runs its whole tier
  through one guest, so a co-resident test's unreaped `Process` perturbs it
  whatever else the host is doing. **Consistent with that mechanism, not
  established as it** — nothing in the capture identifies which process the
  extra object belonged to, and the assertion prints a type and a count rather
  than an owner. Making it print the owner is the cheap next step if this
  recurs, and it is what would turn a consistent story into a measured one.

  **And again 2026-08-18** on `wt/toyos-purecrates`, whose whole delta is three
  kernel files moving into two pure crates with no line of their logic changed —
  the message byte-identical to both above, numbers included: `16 more killed
  processes left more live objects behind: [("Process", 6, 7)]`.
  `cargo test --test toyos-build -- handle_kill_policy` on the same tree
  immediately afterwards: `PASS handle_kill_policy (615ms)`. A third sighting of
  the same census, on a third unrelated branch, is what the mechanism above
  predicts — and the three together are why it is no longer only this file's
  record: `src/redlist.rs` carries an `Instrument::Ci` row for
  `handle_kill_policy` as of the CI sighting, so `cargo run -- --known-red
  handle_kill_policy` now answers it.

- **`wall_clock_file`** — added 2026-08-17, same session, **1 of 6**,
  `ALONE … GREEN`, green on all twelve shards of the same tree. Not
  investigated further.
- **`log_poll_outlives_a_close`** — added 2026-08-17, **1 of 3** full
  `cargo test` runs on `wt/toyos-panicstall`, whose whole delta is the harness's
  panic vocabulary, with a second worktree's suite holding guest slots
  throughout — `toyos-i8042fix` names this very test in that run's
  `[host-slots]` lines. `the close probe exited Some(1)`, `ALONE … GREEN`, and
  `cargo run -- --known-red log_poll_outlives_a_close` answers `NOT ON THE
  LIST`, so this is its first recorded sighting. Not investigated.
- **`metal_sim_input`** — added 2026-08-18, **1 of 4** runs on
  `wt/toyos-lifecycle` (kernel delta: `process_poll_add`'s refusal split and
  `Source::ended_by_its_last_handle`, neither of them on a boot path), inside a
  window whose own console lines are `[build-lock] waiting for the artifact lock
  … 26s so far` — another worktree staging an image throughout. `kernel panic:
  DOUBLE PANIC … after 0 of the sequence`, so it died **before the first
  injection**, which is the boot and not the test. `ALONE … GREEN` in 2 s
  against 20 s under load, then green 3 of 3 more alone.
  `cargo run -- --known-red metal_sim_input` answers `NOT ON THE LIST`.

  **Its mechanism is not this file's census race and it is filed here for want
  of a better register.** A kernel panic during a loaded boot was a filed class
  of its own — five sightings, each carrying the *guest's own panic text* — and
  that class has since been diagnosed and fixed: a CPU could hand a thief the
  task whose context it was still standing on, so two CPUs ran one kernel stack
  (`SchedPass::answer_steal_requests`, `toyos-sched/src/cpu.rs`). This sighting
  carried no console at all — `TestResult::error` held the verdict, a failing
  test's guest console is not printed, and by the time the re-runs were green
  the capture was gone — so it can be neither confirmed as that class nor ruled
  out. **What the next sighting owes is that console.** The way to get one is
  cheaper than a suite: boot `target/bootable.img` twelve at a time in a loop
  and grep each guest's output, which is what reproduced the class above at
  roughly one boot in a hundred.
- **`xhci_full_speed_device`** — added 2026-08-19, **1 of 2** full `cargo test`
  runs on `wt/toyos-clippygate`. `"PANIC:" during the USB gate boot`,
  `ALONE … GREEN`, and `cargo run -- --known-red xhci_full_speed_device`
  answers `NOT ON THE LIST`. The two runs are the measurement: the red one was
  the branch's *first* suite after a full rebuild and its parallel phase took
  103.9 s, the green one minutes later on a warm tree took 32.0 s — so the phase
  that failed was carrying this worktree's own twelve kernel builds, which is
  the load `build_slot` bounds and the section below describes. The branch's
  kernel delta in `drivers/xhci/` is two comment blocks, two `#[allow]`
  attributes and two pairs of parentheses that spell out the precedence Rust
  already applied (`+` and `*` bind tighter than `|`), so no instruction in that
  driver moved. Still `Sched::Parallel`.
- **`diskless_boot`** — added 2026-08-19, one full `cargo test` on
  `wt/toyos-i8042deep`, whose whole delta is host-side harness code
  (`tests/toyos.rs`, `src/redlist.rs`) and no kernel file at all. Twelve wide
  with `toyos-spawnrule`'s suite holding guest slots throughout, named in that
  run's own `[host-slots]` lines. `[qemu] QEMU died before ===READY=== (status:
  Ok(ExitStatus(unix_wait_status(0))))` — **QEMU exited zero**, so this is
  neither a guest that panicked nor a wall-clock guard reporting the content it
  was going to assert; it is the process going away cleanly before the guest was
  ready, which nothing here explains. 7 s under load, `ALONE: GREEN` in 3 s, and
  `cargo run -- --known-red diskless_boot` answered `NOT ON THE LIST`. Not
  investigated.
- **`nvme_large_device`** — same run, same session, and **its mechanism was not
  this file's**: a machine-wide `KERNEL PANIC: execute unmapped address at 0x1b`
  in ring 0 on a `spawn` syscall — the console `metal_sim_input` above owes,
  paid by a different name. That was the stolen-loaded-context defect, since
  diagnosed and fixed, so this bullet is closed: the red was the panic and
  `nvme_large_device` was only the workload it interrupted. `ALONE: GREEN`.
- **`screen_console_shell`** — added 2026-08-19, **1 of 2** full `cargo test`
  runs on `wt/toyos-spawnrule`, 1.23x width with another worktree's suite on the
  host. `typed \`echo zqjxk\` at the prompt and no row of the panel is its
  output` — **786 s** against `PASS (2s)` alone in the same run, and the panel it
  decoded carries only the first frames of boot, so the guest never reached the
  prompt inside the window. Exactly this file's shape: a wall-clock guard
  reporting the content it was going to assert. It is a *different* assertion
  from this name's 2026-08-17 CI row, which is about the seeded `i8042:` line.
  Still `Sched::Parallel`, not investigated.

- **`exit_wait_storm`** — added 2026-08-20, first CI sighting: PR #147 run
  32331741273, `guest (1)`, `timed out after 12s, with the guest still talking
  13s ago (245 console line(s) while it ran) — it was working and did not
  finish`, against a committed price of 200 ms — a 65x wall stretch on a storm
  of exiting children under a loaded shard partition, the `c_capture` shape.
  The same run's `durations` job refused the 13,058 ms reading against the
  10,000 ms fast line, correctly: that number is the partition's, not the
  test's. The diff it rode on is one issue file. `ALONE: GREEN, and it was
  alone both times` — a rate, not a classification; the redlist row carries
  the sighting.

- **`tlb_shootdown_waits`** — added 2026-08-20, **1 of 3** full `cargo test`
  runs on `wt/toyos-p2conv`, with `toyos-dpanic`'s suite holding guest slots
  throughout and named in that run's own `[host-slots]` lines. The other two
  runs were 270/270 on a tree differing from the red one by two doc comments and
  one removed `#[track_caller]`; the branch's kernel delta touches no TLB, no
  shootdown and no `munmap` path. `ALONE … GREEN` in 145 ms, and
  `cargo run -- --known-red tlb_shootdown_waits` answered `NOT ON THE LIST`, so
  this is its first recorded sighting. `screen_early_panic` failed in the same
  run and is already this file's and the redlist's, `ALONE … GREEN` there too.

  **Its shape is this file's, in the one form worth naming separately: the
  assertion that went red is the test's own control.** The message is `munmap
  still took 11740090ns with the delay disarmed, so the numbers above measured
  something other than the wait` — the test arms an injected shootdown delay,
  measures, disarms it, and then requires the *baseline* to be small, because
  that is what proves the armed numbers measured the wait and not the host.
  Which makes it the one assertion in the suite that cannot tell a slow host
  from a broken measurement: on a machine carrying two suites, 11.7 ms for a
  disarmed `munmap` is the load, and the control has no way to say so. Widening
  it is exactly what this file forbids — a control that tolerates 12 ms proves
  nothing about the armed arm either. Making the verdict independent of the rate
  here means comparing armed against disarmed *within the run* rather than each
  against an absolute, which is the first of the two legitimate fix shapes below
  and is the one thing this test already has both samples for. Still
  `Sched::Parallel`, not investigated further.

- **`console_locale_detect`** — added 2026-08-20, first push-triggered `main`
  sighting: `ci` run `32314166262`, `guest (9)`, headSha `eba06ad6`, found
  auditing the merge-health backfill (`issues/build/the-eased-merge-law-carries-a-threshold.md`).
  `STALLED: waiting for the wizard to ask for a key under /bin/console — the
  console did not lend it the keyboard — it never stopped talking and never got
  there`, `ALONE … GREEN` on the harness's own re-run. Same shape as
  `desktop_locale_detect` above — a wizard waiting for a key it was never
  handed — but against `/bin/console` rather than `/bin/terminal`, so it is not
  provably the same boot race and is filed separately. `cargo run --
  --known-red console_locale_detect` answered `NOT ON THE LIST`. Not
  investigated.

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
off. It bounds the build half of a landing gate
by construction, since a gate's builds are these builds. What it does **not**
bound is anything that never enters `src/build.rs` — a `toyos-sched-sim measure`,
a hand-run `cargo build` in a fork clone, the primary's `./x.py`.

**What to do about a red on any of these names:** read the `ALONE` line under it
before anything else. `GREEN` there means the host, not the kernel. What none of
them should get is a widened bound — a gate that tolerates one lost byte
tolerates the defect it was written for. The two fixes above are the two shapes
that are legitimate: make the verdict independent of the rate, or scale a
liveness ceiling with the phase. The global QEMU-slot semaphore this section
used to name as the closing move now exists (`buildlock::guest_slot`): the host
admits twelve guests across every
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
