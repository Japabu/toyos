---
status: open
kind: track
opened: 2026-08-20
---

# The defect-event ledger

`issues/build/the-swarm-is-not-yet-falsifiable.md` asks for raw events before
summaries: "never collapse 'bug found' and 'bug caused' into one count." This
is that raw ledger. **Append-only** — a new event is a new bullet at the
bottom, never an edit to an old one; a later correction to an entry's reading
is its own dated addendum under the entry, the way `src/redlist.rs` disputes a
`Standing` rather than rewriting one.

Three axes, exactly `the-swarm-is-not-yet-falsifiable.md`'s:

- **origin** — `pre-existing` (the tree carried it before the work that found
  it) / `introduced` (the work that found it also caused it) / `unknown`.
- **discoverer** — `implementing agent` (found its own work) / `independent
  agent` (found somebody else's) / `automated gate` (CI, a lint, a compile
  error) / `human` / `runtime observation` (a boot-storm loop, a correlation
  across sightings, not a single test's verdict).
- **escape boundary** — `branch` (never left the agent that found it) / `PR`
  (reached review, not `main`) / `main` (merged, found or fixed after) /
  `release or real hardware`.

One line each below, each with the PR or issue citation the numbers rest on.
Every date is the day the fix or the measurement landed, not the day this
ledger was written.

## Seed rows, 2026-08-19/20

- **The scheduler steal race** — origin: pre-existing. discoverer: runtime
  observation (a boot-storm reproduction loop, correlated across five
  `src/redlist.rs` rows that shared one signature before the mechanism was
  known). escape boundary: `main`, for about eleven days — first sighting
  2026-08-09, fixed by PR #149 ("A CPU may not hand a thief the context it is
  still standing on"), merged 2026-08-20T05:09:53Z. `RunQueue::pop_surplus`
  could hand a thief CPU a task whose saved context this CPU was still
  writing; reproduction went from 13 deaths in 1,272 boots to 0 in 1,584.
  Closed four issue files (`a-ring-0-fetch-at-0x1b-during-a-loaded-boot.md`,
  `a-ring-0-fetch-at-0x1b-with-the-stack-pointer-on-a-page-boundary.md`,
  `a-ring-0-fetch-at-zero-inside-sys-read.md`,
  `the-shared-boot-jumped-to-null-spawning-sched-stress.md`) and retired five
  redlist rows with it.

- **The i8042 byte drops** — origin: pre-existing (QEMU's own PS/2 device, not
  a kernel or harness regression). discoverer: independent agent (diagnosed
  below the guest, below decoding, to the emulator itself — not the agent
  whose branch first tripped the two verdicts). escape boundary: PR (the two
  names reded repeatedly at CI/PR time, each adjudicated `ALONE: GREEN` and
  re-run away, never a `main`-tip red in its own right). PR #143 ("i8042: two
  verdicts red on one shard, and the 26 bytes that met a 16-byte queue"),
  merged 2026-08-19T23:09:34Z: QEMU's PS/2 device holds sixteen bytes
  (`PS2_QUEUE_SIZE`) and past it drops one byte at a time with no signal,
  reproduced deterministically on QEMU 11.1 by putting 22 transitions through
  one `input-send-event`. Closed
  `issues/kernel/two-i8042-verdicts-red-together-on-one-ci-shard.md` and
  `issues/build/i8042-keyboard-pays-a-lost-sentinel-and-reds-the-durations-gate.md`,
  retired two redlist rows.

- **The census lag** — origin: pre-existing. discoverer: automated gate
  (`handle_kill_policy`'s per-kind object census, first CI sighting run
  32047352064, job 95438242676, `guest (2)`, 2026-08-17; cross-witnessed by a
  `SYS_SYSINFO` free-memory reading on the dev host, 2026-08-19). escape
  boundary: `main` — still there, `issues/kernel/deferred-release-outlives-its-syscall.md`
  is open. `object::drain_zero_handles` clears `ZERO_PENDING` before it runs a
  single hook, and any of three drain sites on any CPU can take the batch out
  from under the syscall that queued it — so a killed process can read as
  still holding objects for a visible window, and a process that kills a
  child to make room can hit `ResourceExhausted` for memory the machine is in
  the middle of handing back. Not a leak — free memory always returned to
  baseline — a lag.

- **The mtime cache mis-link** — origin: introduced (by a proposed design, not
  by a landed one). discoverer: implementing agent (its own A/B measurement,
  before the design ever shipped). escape boundary: branch — never reached
  the tree as a feature. PR #137 ("One shared cargo target directory swaps
  one worktree's artifacts for another's, so it is refused"), merged
  2026-08-19T22:44:31Z: the issue asked for one shared `CARGO_TARGET_DIR`
  across worktrees on the premise that cargo keys artifacts on content.
  Measured instead: cargo's path-package freshness is mtime, so a checkout
  whose sources are merely *older* than another checkout's build reads as
  fresh and is never recompiled — two trees sharing a directory produced one
  `.rlib` name and the newer build silently linked the older tree's code, with
  zero diagnostic anywhere. The design did not land; the measurement did.

- **The #140 loom cfg compile error** — origin: introduced (within the same
  PR that later cleared, by an earlier commit of it). discoverer: automated
  gate (`host-tests.yml`'s `kernel-loom --no-default-features` step, one of
  the five required checks). escape boundary: PR — caught and fixed before
  merge, never reached `main`. PR #140 ("Three mechanical debts from the
  2026-08-15 audit, cleared"), merged 2026-08-19T23:39:24Z: commit `7b1a1b17e`
  gated a `MAX_CPUS` pinning assert on `cfg(not(feature = "loom"))`, on the
  mistaken premise that "loom feature off" meant "compiled as the real kernel
  crate" — it does not, `kernel-loom`'s own no-loom build still compiles
  `shootdown.rs` inside the `kernel-loom` crate, where `crate::sched` does not
  exist. `host` went red with `E0433: cannot find sched in the crate root`
  (run 32305989375, job 96238836169); commit `309a7b1` moved the pin to a file
  `kernel-loom` never compiles at all, and the next `host` run was green.

- **The W^X boot flake attribution** — **unresolved as a citation; recorded
  as a gap rather than invented.** The brief for this ledger named this as a
  sixth pre-existing, known-red-row seed alongside the five above. An
  extensive search of `src/redlist.rs`, every `issues/` file, and the PR
  history around W^X's landing (PR #159, "W^X: every user mapping says what
  it is for, and one 4 KiB window per binary is why", merged
  2026-08-20T11:08:27Z, and its groundwork commits `288add2` "EFER joins the
  one declaration, and NXE arrives with it" and `b9dd3f3` "mmap_prot grows the
  other half") found no row or file that attributes a standing boot flake
  (`diskless_boot`, `screen_console_shell`'s `QEMU died before the
  screendump` mode, `kernel_heartbeat`'s clean-exit-before-`===READY===`
  family in `issues/build/qemu-exits-clean-before-ready.md`) to W^X, to the
  NX bit, or to the boot-time CPU-feature assertion at
  `kernel/src/arch/control_regs.rs:238-242` that panics if a CPU lacks the NX
  bit W^X depends on. Whoever placed this row in the brief holds the citation
  this entry is missing; append it here rather than re-deriving it once
  found.

## 2026-08-21

- **`i8042_absent` returned to Fast on one calm sample** — origin: introduced
  (PR #186, the orchestrator reading nightly run 32444411794's 9,221 ms as a
  Cost row crossing back). discoverer: automated gate (the `durations` gate on
  the first two merge-queue compositions after it merged, runs 32475363422
  and 32476143292, both measuring 10,738 ms). escape boundary: `main`, for
  about ninety minutes — merged 2026-08-21T10:29:58Z, every queue entry
  bounced until the reclassification landed. The test's verdict is a 300 ms
  wall-clock comparison and its price straddles the line (10,410 / 9,221 /
  10,738); it was `TimerAnchored` all along. Same-day lesson: a relegation
  table's cost rule invites back whatever one quiet nightly prices under the
  line, and `tests/CLAUDE.md`'s "a widened bound is a finding" has a mirror —
  a narrowed one is too.

- **`i8042_health` reds the `durations` job from either side of the line** —
  origin: introduced (PR #186, the same orchestrator return sweep as the entry
  above, reading nightly run 32444411794's 9,509 ms as a `Why::Cost` row
  crossing back to Fast). discoverer: automated gate (the `durations` job on
  pull request #197, run 32506320411, job 96850003410, 2026-08-21 17:16 UTC,
  measuring 10,281 ms — `guest partition: success` in the same run, so a green
  suite reds a required check). escape boundary: `main`. 9,509 and 10,281 are
  8% apart with the 10,000 ms line between them, so the tier was decided by
  which shard the test landed on, and the red landed on #197 — whose diff is
  soundd's stats line and a mixer counter, nothing i8042 touches.
  `cargo run -- --known-red i8042_health` answered `NOT ON THE LIST`, so every
  author who met it re-derived that from scratch.
  `issues/build/i8042-health-sits-on-the-ten-second-line.md` (on #197's branch)
  is the sighting; the fix is the margin rule below, which relegates it as an
  honest `Why::Cost` row at its committed 9,509 ms.

- **The one-sample tier rule** — origin: pre-existing, and it is the root cause
  of both entries above. discoverer: automated gate — the same `durations` gate,
  three times in one afternoon on three different names (`i8042_absent` runs
  32475363422/32476143292, `i8042_health` run 32506320411,
  `xhci_full_speed_device` run 32513441183), which is what made the pattern
  visible as a rule and not as three tests. escape boundary: `main` — the rule
  had been the tree's since 2026-08-11 and every relegation and return since was
  decided by it. `src/tiers.rs` reded a `Tier::Fast` name measured over
  `FAST_CEILING_MS` and invited a `Why::Cost` row back the moment one
  measurement landed under it, **both on one sample**, so any test priced within
  a few percent of the line was a coin flip per partition and its red landed on
  whichever pull request measured it next. Fixed by the owner's decision of
  2026-08-21 — *the fast tier demands margin* — as `FAST_COMMIT_MS`
  (`FAST_CEILING_MS * 4 / 5` = 8,000 ms): the price a test may be **committed**
  at, refusing a Fast name priced in the band and requiring margin of a returning
  `Why::Cost` row. Three names relegated with it (`i8042_health` 9,509 ms,
  `double_panic_names_the_fault` 9,120 ms, `console_line_atomicity` 8,925 ms);
  the two returns of the same morning were re-checked against the new line and
  keep their Fast (`idle_stack_guard` 5,049 ms, `dump_nmi_probe` 6,284 ms). What
  the fix does *not* cover is
  `issues/build/xhci-full-speed-device-jumped-47-percent-over-its-commitment.md`:
  that name had margin and jumped anyway, which the rule's own derivation says
  is a finding about the test rather than a straddle.
