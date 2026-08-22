---
status: open
kind: track
opened: 2026-08-20
---

# The swarm is not yet falsifiable

The external review of 2026-08-20, adopted by the owner: the swarm has strong
case studies and unusual discipline, and neither is evidence of scaling. The
standing research question is — **does adding another agent reduce
time-to-correct-integrated-software without increasing escaped defects,
coordination cost, integration delay, or human intervention?** Anecdotes and
output volume do not answer it. A standing metrics program does, reviewed on
a fixed cadence.

**Raw events first, summaries second.** For every defect, record:
- origin: pre-existing / introduced by current work / unknown;
- discoverer: implementing agent / independent agent / automated gate /
  human / runtime observation;
- escape boundary: branch / PR / `main` / release or real hardware.

Never collapse "bug found" and "bug caused" into one count: a good swarm
finds many old defects while introducing few; a bad one looks productive by
rapidly finding bugs it just created.

**The minimum metric set:** task-to-merge latency (median, p95); useful
merged changes per week; abandoned or rebuilt PRs; gate rejections; human or
orchestrator interventions per integrated change; agent-introduced defects
caught before merge vs escaping to `main`; pre-existing defects discovered;
integration wait time; cross-PR dependency depth; red-`main` exposure (the
threshold track's numbers); percentage of discoveries converted into durable
tests, invariants, or refusal rules; severity-weighted escaped-defect rate;
and, if feasible, useful integrated change per unit of agent compute.

**Work allocation is part of the same instrument** (review point fifteen):
merged work classified over rolling windows into kernel correctness/security,
verification and tooling, architectural debt reduction, hardware enablement,
user-facing features, and self-hosting/toolchain expansion — with the
standing priority that correctness and security outrank self-hosting, checked
by measurement rather than remembered.

**The reading, defined in advance:** more agents, more throughput, stable
defect and coordination rates — good scaling. Falling defect rates —
exceptional. Rapidly growing integration burden or escaped defects —
saturation. Little added throughput with rising coordination cost — negative
scaling. Whichever the numbers say, the swarm size follows the numbers.

## 2026-08-22 report — the first weekly reading

**Every row `issues/build/defect-events.md` carries, to date.** The ledger
opened 2026-08-20 and its rows run 2026-08-19 through 2026-08-21 — all inside
the week ending today — so this reading covers the ledger's whole life so
far: six events with all three axes filled in, plus one row the ledger itself
marks as an unresolved citation gap (the W^X boot-flake attribution) and
counts nowhere below, by the ledger's own instruction rather than by
omission.<sup>1</sup>

| origin | count | events |
|---|---|---|
| pre-existing | 3 | the scheduler steal race, the i8042 byte drops, the census lag |
| introduced | 3 | the mtime cache mis-link, the #140 loom cfg compile error, `i8042_absent` returned to Fast |
| unknown | 0 | — |

| discoverer | count | events |
|---|---|---|
| automated gate | 3 | the census lag, the #140 loom cfg compile error, `i8042_absent` returned to Fast |
| runtime observation | 1 | the scheduler steal race |
| independent agent | 1 | the i8042 byte drops |
| implementing agent | 1 | the mtime cache mis-link |
| human | 0 | — |

| escape boundary | count | events |
|---|---|---|
| `main` | 3 | the scheduler steal race, the census lag, `i8042_absent` returned to Fast |
| PR | 2 | the i8042 byte drops, the #140 loom cfg compile error |
| branch | 1 | the mtime cache mis-link |
| release or real hardware | 0 | — |

**How many escaped past the boundary they should have been caught at.** Of
the three *introduced* defects — the only axis this question is about, since
a pre-existing defect was already past every boundary before this week's work
touched it — one of three reached `main`: `i8042_absent` returned to Fast
(PR #186), caught not before merge but by the `durations` gate on the first
two merge-queue compositions after it landed, about ninety minutes later. The
other two introduced defects were caught at PR (the #140 loom cfg error,
`host-tests.yml`, never reached `main`) or never left the branch at all (the
mtime cache mis-link, refused before it shipped). Of the three pre-existing
defects, two were already sitting in `main` when found this week (the
scheduler steal race, fixed after about eleven days' exposure; the census lag,
still open) and one was caught at PR every time it recurred and never once
reded `main`'s own tip (the i8042 byte drops).

**The merge-health verdict**, `cargo run -- --merge-health`, queue regime
(`2026-08-20T14:39:48Z .. 2026-08-22T05:31:08Z`, the live regime as of this
reading): **`QUEUE HELD — 4 tip(s) went red on the post-merge push run only;
adjudicate each against src/redlist.rs (a red not on the list is a defect at
its owner, never the queue's)`** — 4 of 32 pushes (12.5 %), all four
composition-success, zero interaction failures. The eased-law part of the
same window (historical, superseded): 16 of 85 pushes (18.8 %),
threshold-breached, already on record in
`issues/build/the-eased-merge-law-carries-a-threshold.md`. Totals across both
regimes: 20 of 117 pushes (17.1 %).<sup>2</sup>

**What the numbers say, against this track's own falsifiability criterion.**
Not enough, yet, to say — and the honest answer is that, not a verdict either
direction. The track asks for a minimum metric set (task-to-merge latency,
merged changes per week, abandoned PRs, gate rejections, interventions,
introduced-vs-pre-existing defect rates, integration wait, cross-PR
dependency depth, red-`main` exposure, conversion to durable tests) measured
"on a fixed cadence" before any scaling verdict is drawn; this ledger, one
week into existing, carries only the three raw axes for six events, and nine
of the eleven named metrics have no row here to compute from yet. What *is*
visible in six events is a shape, not a trend: the swarm found more
pre-existing defects than it introduced-and-lost (3 found old, 3 introduced,
only 1 of those 3 escaped review, and that one was caught by an automated
gate within ninety minutes rather than by a human or by silent persistence to
release) — the shape the track calls a good swarm rather than one that "looks
productive by rapidly finding bugs it just created." The merge-queue read
corroborates rather than contradicts: zero interaction failures in either
regime this week, so the specific composition-breakage risk the queue exists
to catch has not fired. But six events and one week is a description of one
week, not a rate, and the track is explicit that the reading is taken "on a
fixed cadence" — this is the first point on a line that does not yet have a
second one.

<!--
1. `rg -o "origin: [a-z-]+" issues/build/defect-events.md` (6 matches: 3
   pre-existing, 3 introduced); `rg -o "discoverer: [a-z ]+" issues/build/defect-events.md`
   (6 matches, line-wrapped so grouped by hand: automated gate x3, runtime
   observation x1, independent agent x1, implementing agent x1); escape
   boundary read by hand per event, `escape boundary:` itself is line-wrapped
   across two of the six entries so a single-line `rg` under-matches it — the
   W^X row (`## Seed rows, 2026-08-19/20`, last bullet) carries none of the
   three axes and is excluded by the ledger's own text ("unresolved as a
   citation; recorded as a gap rather than invented").
2. `cargo run -- --merge-health`, run 2026-08-22, output in this branch's
   commit; the tool computes the 7-day window ending at its own run time and
   splits it at the merge-queue's first recorded `merge_group` run,
   `2026-08-20T14:39:48Z`.
-->

## 2026-08-22 report — the issue census

**Open issues by kind and status** (every file under `issues/` except
`README.md`, `kind:`/`status:` read from frontmatter, counted after this
session's own edits — three files deleted and one filed, both below):

| kind | status | count |
|---|---|---|
| defect | open | 213 |
| finding | open | 95 |
| track | open | 32 |
| rejected | none | 8 |
| defect | assigned | 7 |
| defect | expected-red | 2 |
| track | assigned | 1 |
| `design-debt` (not a defined `kind`) | open | 1 |
| **total** | | **359** |

By `kind` alone: defect 222, finding 95, track 33, rejected 8, and one file
(`issues/diagnostics/a-console-tag-is-composed-by-replacing-a-bracket.md`)
carrying `kind: design-debt` — one of the ten closed *area* names, not one of
the five defined `kind` values. Filed as
`issues/build/one-issue-file-carries-an-area-name-as-its-kind.md` rather than
fixed, per this repository's own rule against fixing what a bookkeeping pass
finds.<sup>3</sup>

**Opened this week vs. closed/deleted this week.** The README's "Closing
one" section is unambiguous: *"Delete the file. Git keeps the story."* — so a
closed issue is a deletion, verified against this week's own history: the
`--diff-filter=D` events below include the four Ring-0 issue files and the
two i8042 issue files the ledger's seed rows name as closed by PR #149 and PR
#143, confirming the convention holds in practice as well as in the README's
prose. Since `2026-08-15` (a rolling week ending today): **375 `A` (added)
events, 374 unique file paths (one path, `a-killed-peer-still-takes-a-write.md`,
added twice in the window — filed, closed, refiled), across 45 commits; 29
`D` (deleted) events, 29 unique paths, across 18 commits** — plus this
session's own 3 deletions and 1 addition, not yet reflected in the numbers
below since they land in the commit this reading is part of.<sup>4</sup> The
addition count is dominated by one day's restructuring rather than by
new-issue filing: **316 of the 375 additions (297 + 19) are two commits from
2026-08-18** — `issues: the tracker moves to the repository root, and staged
work becomes a kind` and `plans: 8,355 lines of staged intention become 17
issues of kind track` — which moved the tracker into its current
one-file-per-issue shape rather than filing 316 new issues in a week. Set
that structural move aside and **ordinary issue-filing this week is 59
additions across 43 commits** (2026-08-19 through 2026-08-21, one issue per
commit for all but a handful), against **29 deletions across 18 commits** in
the same span — a net of +30 unheld or held files before this session's own
+1/-3.

**`src/redlist.rs` rows**, `KNOWN_RED` (lines 241–2330; 112 `Red` rows,
verified against `rg -n '^    Red \{' src/redlist.rs` restricted to that
range): **76 standing (`Standing::Stands`), 32 retired (`Standing::Retired`),
4 disputed (`Standing::Disputed`)**. Standing rows by instrument: Ci 50,
`DevHostLoaded` 21, `DevHostAlone` 5.<sup>5</sup>

**Open `kind: track` files** — 32 at `status: open`, 1 at `status: assigned`
(`issues/kernel/every-wait-in-this-kernel-is-a-spin.md`):<sup>6</sup>

```
issues/audio/a-client-cannot-tell-soundd-it-paused.md
issues/audio/client-ring-depth-is-the-devices-pipeline-depth.md
issues/audio/hda-has-no-jack-detection-volume-or-keys.md
issues/build/defect-events.md
issues/build/soundds-mix-pass-has-no-host-test.md
issues/build/strict-required-checks-price-a-full-rerun-per-queued-landing.md
issues/build/the-eased-merge-law-carries-a-threshold.md
issues/build/the-initrd-is-still-the-root-filesystem.md
issues/build/the-swarm-is-not-yet-falsifiable.md
issues/build/the-toolchain-ships-no-cargo-and-the-shared-cache-waits-on-one.md
issues/build/there-is-no-network-gate.md
issues/build/toyos-cc-has-never-compiled-tcc.md
issues/design-debt/redesign-the-log-subsystem.md
issues/diagnostics/the-kernel-keeps-nothing-it-enumerates.md
issues/diagnostics/the-log-staged-three-things-it-never-built.md
issues/hardware/a-metal-session-runs-a-pre-flash-gate-first.md
issues/hardware/device-shape-and-lifecycle-have-no-coverage.md
issues/hardware/the-bot-scsi-machine-is-still-hand-written-in-the-kernel.md
issues/hardware/the-t14-touchpad-is-i2c-hid-and-unbuilt.md
issues/hardware/there-is-no-wifi.md
issues/isolation/the-power-broker-authority-with-a-human-in-the-loop.md
issues/kernel/arm64-is-a-decision-nobody-has-made.md
issues/kernel/cpu-time-is-a-band-and-not-a-reservation.md
issues/kernel/every-driver-is-still-in-the-kernel.md
issues/kernel/every-interrupt-lands-on-the-boot-cpu.md
issues/kernel/nothing-charges-kernel-memory-to-a-process.md
issues/kernel/page-global-is-a-decision-nobody-has-made.md
issues/kernel/pcid-tags-are-recycled-live.md
issues/kernel/scheduler-policy-behavior-has-no-quantified-suite.md
issues/kernel/the-capability-end-state-is-twelve-answers.md
issues/kernel/the-iommu-stops-at-translation.md
issues/kernel/the-kernel-still-parses-what-userland-writes.md
```

<!--
3. `rg -o "^kind: .*$" issues/ --no-filename | sort | uniq -c`; cross-tab via
   an `awk` pass reading each file's `status:`/`kind:` frontmatter pair,
   `find issues -type f -name '*.md' ! -name 'README.md' | wc -l` = 359 for
   the total.
4. `git log --since=2026-08-15 --diff-filter=A --name-only -- issues/` and
   the same with `--diff-filter=D`, each with `--format='COMMIT %H %ad %s'`
   to attribute per commit; unique-path count via `sort -u` after stripping
   `README.md` and commit-header lines.
5. `rg -n "^\];" src/redlist.rs` to find `KNOWN_RED`'s close (line 2330);
   `rg -n "standing: Standing::Stands," / "standing: Standing::Retired(" /
   "standing: Standing::Disputed(" src/redlist.rs` restricted by line number
   to 241–2330 (the six matching lines at or past 2713 are `mod tests`
   fixtures, not `KNOWN_RED` rows, and are excluded); per-instrument
   breakdown via an `awk` pass pairing each row's `instrument:` and
   `standing:` field between successive `Red {` openers.
6. `comm -12 <(rg -l '^kind: track$' issues/ | sort) <(rg -l '^status: open$' issues/ | sort)`
   and the same against `^status: assigned$`.
-->

